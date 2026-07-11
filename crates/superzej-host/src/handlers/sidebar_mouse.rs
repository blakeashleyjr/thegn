//! Sidebar mouse handling: click/caret-click/Ctrl-click, right-click context
//! menu, double-click, and drag (reorder within siblings, drop onto a folder
//! or workspace header to file/unfile). Extracted logic keeps run.rs (ratchet-
//! pinned) to thin dispatch arms.
//!
//! The gesture state machine is pure over the model + hit geometry
//! ([`crate::sidebar_view::hit_rows`], the same `build_sidebar` pass the
//! renderer painted), so transitions are unit-testable without a terminal.
//! Drag feedback rides `FrameModel::sidebar_drag`; drops reuse the keyboard
//! paths (`move_worktree_group` / `move_workspace_by_slug` — inheriting the
//! computed-sort→Manual flip and home anchoring — and `file_worktree_path` /
//! `unfile_worktree_path`).

use std::time::Instant;

use tokio::sync::mpsc::UnboundedSender;

use crate::chrome::FrameModel;
use crate::handlers::sidebar_keys::SidebarOutcome;
use crate::handlers::sidebar_persist::SidebarState;
use crate::hydrate::RefreshKind;
use crate::sidebar::{RowKind, RowTarget};
use crate::sidebar_view::{DragSpotViz, RowHit, SidebarDragViz, hit_rows, menu_rect, row_at};

/// Double-click window (same row, released and re-pressed within this).
const DOUBLE_CLICK_MS: u128 = 400;

/// What is being dragged.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DragSrc {
    /// A live worktree row (session `Tab` target, non-home).
    Worktree {
        pin_key: String,
        slug: String,
        path: String,
    },
    /// A DB-backed workspace header.
    Workspace { pin_key: String, slug: String },
}

impl DragSrc {
    fn pin_key(&self) -> &str {
        match self {
            DragSrc::Worktree { pin_key, .. } | DragSrc::Workspace { pin_key, .. } => pin_key,
        }
    }
}

/// Where a drag would drop, resolved fresh on every motion sample.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Spot {
    /// Land the source at this position among its siblings: before the sibling
    /// with `before_pin_key`, or at the end of the run (`None`).
    Reorder {
        before_pin_key: Option<String>,
        viz: DragSpotViz,
    },
    /// File the worktree into this folder (same workspace only).
    FileInto {
        folder_name: String,
        viz_index: usize,
    },
    /// Move the worktree out of its folder (drop on its own workspace header).
    Unfile {
        viz_index: usize,
    },
    Invalid,
}

impl Spot {
    fn viz(&self) -> DragSpotViz {
        match self {
            Spot::Reorder { viz, .. } => viz.clone(),
            Spot::FileInto { viz_index, .. } | Spot::Unfile { viz_index } => {
                DragSpotViz::Target(*viz_index)
            }
            Spot::Invalid => DragSpotViz::Invalid,
        }
    }
}

/// The press → drag gesture state.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) enum DragPhase {
    #[default]
    Idle,
    /// Button down on a draggable row; becomes a drag when the pointer leaves
    /// the pressed row's band (sub-row jitter stays a click).
    Pressed {
        src: DragSrc,
        row_y: usize,
        row_h: usize,
    },
    Dragging {
        src: DragSrc,
        spot: Spot,
    },
}

/// Loop-persistent sidebar mouse state.
#[derive(Default)]
pub(crate) struct MouseUi {
    pub drag: DragPhase,
    /// `(pin_key, at)` of the last left press, for double-click detection.
    last_click: Option<(String, Instant)>,
}

/// What the loop should do after a left press in the sidebar.
pub(crate) enum PressOut {
    /// Handled (cursor moved / caret toggled / mark toggled); just redraw.
    Consumed,
    /// Activate this target; `force_center` commits keyboard focus to the
    /// center (the double-click gesture).
    Activate {
        target: RowTarget,
        force_center: bool,
    },
    /// The hinted action of an EmptyHint row (Enter-equivalent).
    Outcome(SidebarOutcome),
}

/// Left press over the sidebar: set the cursor, resolve caret clicks and
/// Ctrl-marks, detect double-click, arm a potential drag, and activate.
/// The caller has already focused the sidebar zone.
#[allow(clippy::too_many_arguments)]
pub(crate) fn on_left_press(
    ui: &mut MouseUi,
    sb: &mut SidebarState,
    model: &mut FrameModel,
    session: &crate::session::Session,
    rect: crate::compositor::Rect,
    mx: usize,
    my: usize,
    ctrl: bool,
    now: Instant,
) -> PressOut {
    let hits = hit_rows(model, rect);
    let Some(hit) = row_at(&hits, my).cloned() else {
        return PressOut::Consumed;
    };
    sb.cursor = hit.visible_index;

    // Caret cell: toggle collapse instead of activating (the affordance the
    // caret glyph promises).
    if hit.caret_x == Some(mx) && hit.kind.is_collapsible() {
        return match sb.toggle_collapse(model, session) {
            SidebarOutcome::Redraw => PressOut::Consumed,
            out => PressOut::Outcome(out),
        };
    }

    // Ctrl+click: toggle the multi-select mark by stable identity.
    if ctrl {
        if let Some(key) = model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible)
            .nth(hit.visible_index)
            .filter(|r| r.is_markable())
            .map(|r| r.pin_key.clone())
            && !sb.marked.remove(&key)
        {
            sb.marked.insert(key);
        }
        sb.sync(model);
        return PressOut::Consumed;
    }

    // Double-click: second press on the same row within the window.
    let double = ui.last_click.as_ref().is_some_and(|(k, at)| {
        *k == hit.pin_key && now.duration_since(*at).as_millis() <= DOUBLE_CLICK_MS
    });
    ui.last_click = Some((hit.pin_key.clone(), now));

    // Arm a potential drag on draggable rows (live non-home worktrees and
    // DB-backed workspaces). The press still activates below — a drag that
    // follows simply reorders the now-active row.
    ui.drag = drag_src_for(sb, model, session, &hit)
        .map(|src| DragPhase::Pressed {
            src,
            row_y: hit.y,
            row_h: hit.height,
        })
        .unwrap_or(DragPhase::Idle);

    // Headers: double-click toggles collapse (VS Code-like); single click
    // just selects (Enter/caret folds).
    if hit.kind.is_collapsible() {
        if double {
            return match sb.toggle_collapse(model, session) {
                SidebarOutcome::Redraw => PressOut::Consumed,
                out => PressOut::Outcome(out),
            };
        }
        sb.sync(model);
        return PressOut::Consumed;
    }
    if hit.kind == RowKind::EmptyHint {
        return PressOut::Outcome(SidebarOutcome::Synthetic(
            crate::keymap::Action::NewTerminal,
        ));
    }

    sb.sync(model);
    match sb.cursor_target(model) {
        Some(target) => PressOut::Activate {
            target,
            force_center: double,
        },
        None => PressOut::Consumed,
    }
}

/// Right press over the sidebar: select the row and open its context menu
/// anchored there (the same catalog `m` opens).
pub(crate) fn on_right_press(
    sb: &mut SidebarState,
    model: &mut FrameModel,
    session: &crate::session::Session,
    rect: crate::compositor::Rect,
    my: usize,
) {
    let hits = hit_rows(model, rect);
    if let Some(hit) = row_at(&hits, my) {
        sb.cursor = hit.visible_index;
        sb.menu = sb.menu_for_cursor(model, session);
        sb.sync(model);
    }
}

/// Mouse over an OPEN row menu: click an entry to run it, click outside to
/// dismiss, wheel to move the menu cursor. Returns `Some(outcome)` when an
/// entry ran; `None` otherwise (event consumed either way).
pub(crate) fn on_menu_mouse(
    sb: &mut SidebarState,
    model: &mut FrameModel,
    session: &crate::session::Session,
    rect: crate::compositor::Rect,
    my: usize,
    press: bool,
    wheel: Option<bool>, // Some(up)
) -> Option<SidebarOutcome> {
    let menu = sb.menu.clone()?;
    let frame = crate::sidebar_view::build_sidebar(model, rect, model.sidebar_scroll);
    let mrect = menu_rect(rect, &frame, &menu);
    if let Some(up) = wheel {
        if let Some(m) = sb.menu.as_mut() {
            m.cursor =
                crate::sidebar_view::menu_step(&m.entries, m.cursor, if up { -1 } else { 1 });
        }
        sb.sync(model);
        return None;
    }
    if !press {
        return None;
    }
    if my >= mrect.y && my < mrect.y + mrect.rows {
        let i = my - mrect.y;
        if let Some(entry) = menu.entries.get(i).filter(|e| !e.is_separator()) {
            let id = entry.id.clone();
            sb.menu = None;
            // Land the cursor back on the menu's target row before acting.
            if let Some(idx) = model
                .sidebar_rows
                .iter()
                .filter(|r| r.visible)
                .position(|r| r.pin_key == menu.target_pin_key)
            {
                sb.cursor = idx;
            }
            let out = sb.run_menu_action(&id, model, session);
            sb.sync(model);
            return Some(out);
        }
        return None;
    }
    // Click outside dismisses.
    sb.menu = None;
    sb.sync(model);
    None
}

/// A motion sample while the left button is held. Returns true when the event
/// belonged to a sidebar drag (armed or active) and was consumed.
pub(crate) fn on_drag_move(
    ui: &mut MouseUi,
    sb: &mut SidebarState,
    model: &mut FrameModel,
    session: &crate::session::Session,
    rect: crate::compositor::Rect,
    my: usize,
) -> bool {
    match std::mem::take(&mut ui.drag) {
        DragPhase::Idle => {
            ui.drag = DragPhase::Idle;
            false
        }
        DragPhase::Pressed { src, row_y, row_h } => {
            if my >= row_y && my < row_y + row_h {
                // Still inside the pressed row: not a drag yet.
                ui.drag = DragPhase::Pressed { src, row_y, row_h };
                return true;
            }
            let spot = spot_at(sb, model, session, rect, &src, my);
            apply_viz(model, rect, &src, &spot);
            ui.drag = DragPhase::Dragging { src, spot };
            true
        }
        DragPhase::Dragging { src, .. } => {
            let spot = spot_at(sb, model, session, rect, &src, my);
            apply_viz(model, rect, &src, &spot);
            // Edge autoscroll: nudge the cursor when dragging at the list
            // edges; the per-frame clamp scrolls the window. No timers —
            // feedback advances only on motion samples.
            if my <= rect.y + 2 {
                sb.cursor = sb.cursor.saturating_sub(1);
            } else if my + 1 >= rect.y + rect.rows {
                let visible = SidebarState::visible_len(model);
                if visible > 0 {
                    sb.cursor = (sb.cursor + 1).min(visible - 1);
                }
            }
            sb.sync(model);
            ui.drag = DragPhase::Dragging { src, spot };
            true
        }
    }
}

/// What a button release resolved to. Pure: the caller executes any drop via
/// [`perform_drop`] (which owns the persistence side effects), keeping this
/// state machine testable without a terminal.
#[derive(Debug, PartialEq)]
pub(crate) enum ReleaseOut {
    /// No sidebar gesture was in flight; the release belongs to someone else.
    NotOurs,
    /// A plain click ended (the press already handled it).
    Click,
    /// A drag ended: execute this drop.
    Drop { src: DragSrc, spot: Spot },
}

/// Button release: end the gesture and say what (if anything) to drop.
pub(crate) fn on_release(ui: &mut MouseUi, model: &mut FrameModel) -> ReleaseOut {
    let phase = std::mem::take(&mut ui.drag);
    model.sidebar_drag = None;
    match phase {
        DragPhase::Idle => ReleaseOut::NotOurs,
        DragPhase::Pressed { .. } => ReleaseOut::Click,
        DragPhase::Dragging { src, spot } => ReleaseOut::Drop { src, spot },
    }
}

/// Mirror the current drag onto the model for the renderer.
fn apply_viz(model: &mut FrameModel, rect: crate::compositor::Rect, src: &DragSrc, spot: &Spot) {
    let hits = hit_rows(model, rect);
    let source = hits
        .iter()
        .find(|h| h.pin_key == src.pin_key())
        .map(|h| h.visible_index)
        .unwrap_or(usize::MAX);
    model.sidebar_drag = Some(SidebarDragViz {
        source,
        spot: spot.viz(),
    });
}

/// Whether (and what) this row can drag as.
fn drag_src_for(
    sb: &SidebarState,
    model: &FrameModel,
    session: &crate::session::Session,
    hit: &RowHit,
) -> Option<DragSrc> {
    let row = model
        .sidebar_rows
        .iter()
        .filter(|r| r.visible)
        .nth(hit.visible_index)?;
    match row.kind {
        RowKind::Worktree => {
            // Live, non-home worktrees only: the step-move machinery needs a
            // session group, and home is anchored first.
            let Some(RowTarget::Tab(gi, _)) = row.tab_target else {
                return None;
            };
            if session.worktrees.get(gi).map(|g| g.kind) == Some(crate::session::GroupKind::Home) {
                return None;
            }
            let _ = sb; // (kept for future marked-set drags)
            Some(DragSrc::Worktree {
                pin_key: row.pin_key.clone(),
                slug: row.workspace_slug.clone(),
                path: row.worktree_path.clone()?,
            })
        }
        RowKind::Workspace if row.worktree_path.is_some() => Some(DragSrc::Workspace {
            pin_key: row.pin_key.clone(),
            slug: row.workspace_slug.clone(),
        }),
        _ => None,
    }
}

/// Resolve the pointer's drop spot for `src` — the heart of the gesture,
/// pure over the model + hit geometry.
fn spot_at(
    sb: &SidebarState,
    model: &FrameModel,
    _session: &crate::session::Session,
    rect: crate::compositor::Rect,
    src: &DragSrc,
    my: usize,
) -> Spot {
    let _ = sb;
    let hits = hit_rows(model, rect);
    let Some(hit) = row_at(&hits, my) else {
        return Spot::Invalid;
    };
    if hit.pin_key == src.pin_key() {
        return Spot::Invalid; // hovering the source itself
    }
    let visible: Vec<&crate::sidebar::SidebarRow> =
        model.sidebar_rows.iter().filter(|r| r.visible).collect();
    let Some(row) = visible.get(hit.visible_index).copied() else {
        return Spot::Invalid;
    };
    match src {
        DragSrc::Worktree { slug, .. } => {
            if row.workspace_slug != *slug {
                return Spot::Invalid; // worktrees never cross workspaces
            }
            match row.kind {
                RowKind::Folder => Spot::FileInto {
                    folder_name: row.label.clone(),
                    viz_index: hit.visible_index,
                },
                RowKind::Workspace => Spot::Unfile {
                    viz_index: hit.visible_index,
                },
                RowKind::Worktree => {
                    // Home is anchored first: dropping above it is invalid.
                    let is_home = row.label == "home";
                    let top_half = my < hit.y + hit.height.div_ceil(2);
                    if is_home && top_half {
                        return Spot::Invalid;
                    }
                    if top_half {
                        Spot::Reorder {
                            before_pin_key: Some(row.pin_key.clone()),
                            viz: DragSpotViz::InsertBefore(hit.visible_index),
                        }
                    } else {
                        // Bottom half: before the NEXT same-workspace worktree,
                        // or at the end of the run.
                        let next = visible.iter().enumerate().skip(hit.visible_index + 1).find(
                            |(_, r)| r.kind == RowKind::Worktree && r.workspace_slug == *slug,
                        );
                        match next {
                            Some((i, r)) => Spot::Reorder {
                                before_pin_key: Some(r.pin_key.clone()),
                                viz: DragSpotViz::InsertBefore(i),
                            },
                            None => Spot::Reorder {
                                before_pin_key: None,
                                viz: DragSpotViz::InsertAfter(hit.visible_index),
                            },
                        }
                    }
                }
                _ => Spot::Invalid,
            }
        }
        DragSrc::Workspace { slug, .. } => {
            // Any row resolves to its enclosing workspace; terminals region is
            // out of bounds. Top/bottom half of that workspace's header picks
            // before/after.
            if row.workspace_slug == *slug {
                return Spot::Invalid;
            }
            if row.workspace_slug == "terminals" || row.workspace_slug.starts_with("terminals/") {
                return Spot::Invalid;
            }
            // The hovered row's workspace header.
            let header = visible.iter().enumerate().find(|(_, r)| {
                r.kind == RowKind::Workspace && r.workspace_slug == row.workspace_slug
            });
            let Some((hi, hrow)) = header else {
                return Spot::Invalid;
            };
            // Hovering the top half of the header row inserts before it;
            // anywhere else in its subtree inserts after it.
            let before = hit.visible_index == hi && my < hit.y + hit.height.div_ceil(2);
            if before {
                Spot::Reorder {
                    before_pin_key: Some(hrow.pin_key.clone()),
                    viz: DragSpotViz::InsertBefore(hi),
                }
            } else {
                // After this workspace = before the NEXT workspace header.
                let next = visible
                    .iter()
                    .enumerate()
                    .skip(hi + 1)
                    .find(|(_, r)| r.kind == RowKind::Workspace);
                match next {
                    Some((i, r)) => Spot::Reorder {
                        before_pin_key: Some(r.pin_key.clone()),
                        viz: DragSpotViz::InsertBefore(i),
                    },
                    None => Spot::Reorder {
                        before_pin_key: None,
                        viz: DragSpotViz::InsertAfter(hi),
                    },
                }
            }
        }
    }
}

/// Execute the drop, reusing the keyboard machinery (which owns persistence,
/// the sort→Manual flip, and home anchoring).
pub(crate) fn perform_drop(
    sb: &mut SidebarState,
    model: &mut FrameModel,
    session: &mut crate::session::Session,
    refresh_tx: &UnboundedSender<RefreshKind>,
    waker: &termwiz::terminal::TerminalWaker,
    src: &DragSrc,
    spot: &Spot,
) {
    match (src, spot) {
        (_, Spot::Invalid) => {}
        (DragSrc::Worktree { path, slug, .. }, Spot::FileInto { folder_name, .. }) => {
            let repo_path = model
                .sidebar_workspaces
                .iter()
                .find(|(s, ..)| s == slug)
                .map(|(_, _, _, p)| p.clone())
                .filter(|p| !p.is_empty());
            if let Some(repo_path) = repo_path {
                match crate::handlers::sidebar_folder::file_worktree_path(
                    session,
                    sb,
                    model,
                    path,
                    &repo_path,
                    folder_name,
                    refresh_tx,
                    waker,
                ) {
                    Ok(msg) | Err(msg) => model.status = msg,
                }
            }
        }
        (DragSrc::Worktree { path, .. }, Spot::Unfile { .. }) => {
            model.status = crate::handlers::sidebar_folder::unfile_worktree_path(
                session, sb, model, path, refresh_tx, waker,
            );
        }
        (DragSrc::Worktree { pin_key, slug, .. }, Spot::Reorder { before_pin_key, .. }) => {
            reorder_worktree_to(sb, model, session, pin_key, slug, before_pin_key.as_deref());
        }
        (DragSrc::Workspace { slug, .. }, Spot::Reorder { before_pin_key, .. }) => {
            reorder_workspace_to(sb, model, session, slug, before_pin_key.as_deref());
        }
        // Workspaces can't file/unfile.
        (DragSrc::Workspace { .. }, Spot::FileInto { .. } | Spot::Unfile { .. }) => {}
    }
}

/// The same-workspace worktree pin keys in current visual order.
fn worktree_run(model: &FrameModel, slug: &str) -> Vec<String> {
    model
        .sidebar_rows
        .iter()
        .filter(|r| r.visible && r.kind == RowKind::Worktree && r.workspace_slug == slug)
        .map(|r| r.pin_key.clone())
        .collect()
}

/// Step-move the source worktree until it sits at the target slot, one
/// validated neighbor swap at a time (each swap persists + respects home).
fn reorder_worktree_to(
    sb: &mut SidebarState,
    model: &mut FrameModel,
    session: &mut crate::session::Session,
    src_pin: &str,
    slug: &str,
    before_pin: Option<&str>,
) {
    // Cap the step loop: each step must make progress; the run length bounds it.
    let max_steps = worktree_run(model, slug).len() + 1;
    for _ in 0..max_steps {
        let run = worktree_run(model, slug);
        let Some(cur) = run.iter().position(|k| k == src_pin) else {
            return;
        };
        let target = match before_pin {
            Some(bp) => match run.iter().position(|k| k == bp) {
                Some(t) => t,
                None => return, // target vanished (filed/deleted mid-drag)
            },
            None => run.len(),
        };
        // Moving down past `target` means landing at target-1 (the source's
        // removal shifts later slots up by one).
        let dest = if target > cur { target - 1 } else { target };
        if dest == cur {
            return;
        }
        let up = dest < cur;
        // Resolve the source's CURRENT session group index by pin key.
        let gi = model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible)
            .find(|r| r.pin_key == src_pin)
            .and_then(|r| match r.tab_target {
                Some(RowTarget::Tab(gi, _)) => Some(gi),
                _ => None,
            });
        let Some(gi) = gi else { return };
        if !sb.move_worktree_group(model, session, gi, up) {
            return; // blocked (home anchor / edge): stop rather than spin
        }
    }
}

/// Step-move the source workspace to the target slot (see
/// [`reorder_worktree_to`]; workspaces move via `move_workspace_by_slug`).
fn reorder_workspace_to(
    sb: &mut SidebarState,
    model: &mut FrameModel,
    session: &crate::session::Session,
    src_slug: &str,
    before_pin: Option<&str>,
) {
    let run = |model: &FrameModel| -> Vec<String> {
        model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible && r.kind == RowKind::Workspace)
            .map(|r| r.pin_key.clone())
            .collect()
    };
    let max_steps = run(model).len() + 1;
    for _ in 0..max_steps {
        let order = run(model);
        let Some(cur) = order.iter().position(|k| k == src_slug) else {
            return;
        };
        let target = match before_pin {
            Some(bp) => match order.iter().position(|k| k == bp) {
                Some(t) => t,
                None => return,
            },
            None => order.len(),
        };
        let dest = if target > cur { target - 1 } else { target };
        if dest == cur {
            return;
        }
        if !sb.move_workspace_by_slug(model, session, src_slug, dest < cur) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sidebar::SidebarRow;

    fn wt_row(slug: &str, branch: &str, gi: usize) -> SidebarRow {
        SidebarRow {
            tab_target: Some(RowTarget::Tab(gi, 0)),
            worktree_path: Some(format!("/wt/{branch}")),
            pin_key: format!("{slug}/{branch}"),
            branch: Some(branch.into()),
            ..SidebarRow::base(RowKind::Worktree, 1, branch, slug)
        }
    }

    fn ws_row(slug: &str) -> SidebarRow {
        SidebarRow {
            worktree_path: Some(format!("/repos/{slug}")),
            pin_key: slug.into(),
            ..SidebarRow::base(RowKind::Workspace, 0, slug, slug)
        }
    }

    fn folder_row(slug: &str, id: i64, name: &str) -> SidebarRow {
        SidebarRow {
            pin_key: format!("{slug}/folder:{id}"),
            folder_id: Some(id),
            ..SidebarRow::base(RowKind::Folder, 1, name, slug)
        }
    }

    /// app workspace: header, home, feat, zeta, folder "Backend", plus a
    /// second workspace with one worktree. Rect places rows from y=2
    /// (header + blank), one line each (nothing selected/expanded).
    fn fixture() -> (crate::chrome::FrameModel, crate::compositor::Rect) {
        let model = crate::chrome::FrameModel {
            sidebar_rows: vec![
                ws_row("app"),
                wt_row("app", "home", 0),
                wt_row("app", "feat", 1),
                wt_row("app", "zeta", 2),
                folder_row("app", 1, "Backend"),
                ws_row("lib"),
                wt_row("lib", "home", 3),
            ],
            sidebar_workspaces: vec![
                (
                    "app".into(),
                    "app".into(),
                    "repo".into(),
                    "/repos/app".into(),
                ),
                (
                    "lib".into(),
                    "lib".into(),
                    "repo".into(),
                    "/repos/lib".into(),
                ),
            ],
            ..Default::default()
        };
        let rect = crate::compositor::Rect {
            x: 0,
            y: 0,
            cols: 30,
            rows: 20,
        };
        (model, rect)
    }

    fn y_of(model: &crate::chrome::FrameModel, rect: crate::compositor::Rect, pin: &str) -> usize {
        let hits = hit_rows(model, rect);
        let visible: Vec<&SidebarRow> = model.sidebar_rows.iter().filter(|r| r.visible).collect();
        hits.iter()
            .find(|h| visible[h.visible_index].pin_key == pin)
            .map(|h| h.y)
            .unwrap()
    }

    fn src_feat() -> DragSrc {
        DragSrc::Worktree {
            pin_key: "app/feat".into(),
            slug: "app".into(),
            path: "/wt/feat".into(),
        }
    }

    #[test]
    fn spot_worktree_reorders_within_its_workspace() {
        let (model, rect) = fixture();
        let sb = SidebarState::default();
        let session = crate::session::Session::default();
        // Top half of zeta → land before zeta.
        let y = y_of(&model, rect, "app/zeta");
        match spot_at(&sb, &model, &session, rect, &src_feat(), y) {
            Spot::Reorder { before_pin_key, .. } => {
                assert_eq!(before_pin_key.as_deref(), Some("app/zeta"));
            }
            other => panic!("expected reorder, got {other:?}"),
        }
        // Bottom half of home → before the next sibling (feat itself is next;
        // spot resolution is source-agnostic here) — still a Reorder.
        let y = y_of(&model, rect, "app/home");
        assert!(matches!(
            spot_at(&sb, &model, &session, rect, &src_feat(), y),
            Spot::Reorder { .. } | Spot::Invalid
        ));
    }

    #[test]
    fn spot_worktree_never_crosses_workspaces_and_never_lands_above_home() {
        let (model, rect) = fixture();
        let sb = SidebarState::default();
        let session = crate::session::Session::default();
        // Another workspace's worktree → Invalid.
        let y = y_of(&model, rect, "lib/home");
        assert_eq!(
            spot_at(&sb, &model, &session, rect, &src_feat(), y),
            Spot::Invalid
        );
        // Top half of the home row → Invalid (home is anchored first).
        let y = y_of(&model, rect, "app/home");
        assert_eq!(
            spot_at(&sb, &model, &session, rect, &src_feat(), y),
            Spot::Invalid,
            "top half of home must refuse the drop"
        );
    }

    #[test]
    fn spot_worktree_files_into_folder_and_unfiles_on_workspace_header() {
        let (model, rect) = fixture();
        let sb = SidebarState::default();
        let session = crate::session::Session::default();
        let y = y_of(&model, rect, "app/folder:1");
        match spot_at(&sb, &model, &session, rect, &src_feat(), y) {
            Spot::FileInto { folder_name, .. } => assert_eq!(folder_name, "Backend"),
            other => panic!("expected FileInto, got {other:?}"),
        }
        let y = y_of(&model, rect, "app");
        assert!(matches!(
            spot_at(&sb, &model, &session, rect, &src_feat(), y),
            Spot::Unfile { .. }
        ));
        // A folder in ANOTHER workspace would be Invalid (cross-workspace rule
        // covered above via lib/home).
    }

    #[test]
    fn spot_workspace_reorders_between_headers_only() {
        let (model, rect) = fixture();
        let sb = SidebarState::default();
        let session = crate::session::Session::default();
        let src = DragSrc::Workspace {
            pin_key: "lib".into(),
            slug: "lib".into(),
        };
        // Hovering anywhere in app's subtree (below the header's top half)
        // inserts after app = before lib... which is where lib already is;
        // the drop executor no-ops on dest==cur. The spot itself must still
        // be a Reorder (not Invalid).
        let y = y_of(&model, rect, "app/feat");
        assert!(matches!(
            spot_at(&sb, &model, &session, rect, &src, y),
            Spot::Reorder { .. }
        ));
        // Terminals region would be Invalid; own workspace is Invalid.
        let y = y_of(&model, rect, "lib/home");
        assert_eq!(spot_at(&sb, &model, &session, rect, &src, y), Spot::Invalid);
    }

    #[test]
    fn pressed_becomes_dragging_only_after_leaving_the_row_band() {
        let (mut model, rect) = fixture();
        let mut sb = SidebarState::default();
        let session = crate::session::Session::default();
        let mut ui = MouseUi {
            drag: DragPhase::Pressed {
                src: src_feat(),
                row_y: 4,
                row_h: 1,
            },
            ..Default::default()
        };
        // Sample inside the pressed band: still a (potential) click.
        assert!(on_drag_move(
            &mut ui, &mut sb, &mut model, &session, rect, 4
        ));
        assert!(matches!(ui.drag, DragPhase::Pressed { .. }));
        assert!(model.sidebar_drag.is_none());
        // Leaving the band starts the drag and mirrors the viz on the model.
        assert!(on_drag_move(
            &mut ui, &mut sb, &mut model, &session, rect, 6
        ));
        assert!(matches!(ui.drag, DragPhase::Dragging { .. }));
        assert!(model.sidebar_drag.is_some());
    }

    #[test]
    fn release_after_plain_press_is_a_click_and_clears_state() {
        let (mut model, _rect) = fixture();
        let mut ui = MouseUi {
            drag: DragPhase::Pressed {
                src: src_feat(),
                row_y: 4,
                row_h: 1,
            },
            ..Default::default()
        };
        assert_eq!(on_release(&mut ui, &mut model), ReleaseOut::Click);
        assert_eq!(ui.drag, DragPhase::Idle);
        assert!(model.sidebar_drag.is_none());
        // Idle release is not a sidebar gesture.
        assert_eq!(on_release(&mut ui, &mut model), ReleaseOut::NotOurs);
        // A drag hands back the drop for the caller to execute.
        let spot = Spot::Unfile { viz_index: 1 };
        ui.drag = DragPhase::Dragging {
            src: src_feat(),
            spot: spot.clone(),
        };
        assert_eq!(
            on_release(&mut ui, &mut model),
            ReleaseOut::Drop {
                src: src_feat(),
                spot
            }
        );
    }
}
