//! Sidebar mouse handling: click/caret-click/Ctrl-click, right-click context
//! menu, double-click, and drag (reorder within siblings, drop onto a folder
//! or workspace header to file/unfile). Extracted logic keeps run.rs (ratchet-
//! pinned) to thin dispatch arms.
//!
//! The gesture state machine is pure over the model + hit geometry
//! ([`crate::sidebar_view::hit_rows`], the same `build_sidebar` pass the
//! renderer painted), so transitions are unit-testable without a terminal.
//! Drag feedback rides `FrameModel::sidebar_drag`; drops reuse the keyboard
//! paths (`apply_order_plan` / `apply_folder_order` / `move_workspace_by_slug` —
//! inheriting the computed-sort→Manual flip and home anchoring — and
//! `file_worktree_path` / `unfile_worktree_path`).
//!
//! Worktree drops resolve through [`crate::sidebar_order`], so a release
//! *between two rows inside a folder* files the worktree into that folder and
//! positions it there in one write. The drop is a single resolved plan, not the
//! bounded step-swap loop this used to run — that loop stepped through a flat,
//! folder-blind run and could bail out mid-way after churning other rows.

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
    /// A folder header — reorders among its workspace's folders.
    Folder {
        pin_key: String,
        slug: String,
        folder_id: i64,
    },
}

impl DragSrc {
    fn pin_key(&self) -> &str {
        match self {
            DragSrc::Worktree { pin_key, .. }
            | DragSrc::Workspace { pin_key, .. }
            | DragSrc::Folder { pin_key, .. } => pin_key,
        }
    }
}

/// Where a drag would drop, resolved fresh on every motion sample.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Spot {
    /// Land the source at this position among its siblings: before the sibling
    /// with `before_pin_key`, or at the end of the run (`None`).
    ///
    /// `folder` is the **destination run** for a worktree drag — `None` for the
    /// loose list, `Some(id)` for a folder. Carrying it here is what lets a drop
    /// *between two rows inside a folder* both file and position the worktree;
    /// without it the drop reordered a flat workspace-wide run and the renderer
    /// put the row straight back where it came from.
    Reorder {
        before_pin_key: Option<String>,
        folder: Option<i64>,
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
        // The rail paints no menu overlay; opening one there arms an
        // invisible modal that swallows every key (same guard as `m`).
        if model.sidebar_rail {
            model.status = "The menu needs the full sidebar — Alt-s to expand".into();
        } else {
            sb.menu = sb.menu_for_cursor(model, session);
        }
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
            // Land the cursor back on the menu's target row before acting. If
            // that row vanished while the menu was up (hydration prune,
            // re-file re-keying it), bail — acting would fire the entry
            // (possibly Delete) at whatever row the cursor happens to be on.
            let Some(idx) = model
                .sidebar_rows
                .iter()
                .filter(|r| r.visible)
                .position(|r| r.pin_key == menu.target_pin_key)
            else {
                model.status = "That row is gone — menu closed".into();
                sb.sync(model);
                return Some(SidebarOutcome::Redraw);
            };
            sb.cursor = idx;
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
        // A folder header drags among its workspace's folders; its worktrees
        // travel with it. Optimistically-created folders carry a synthetic
        // negative id with no DB row yet, so they can't be reordered until the
        // deferred filing write assigns the real one.
        RowKind::Folder => row
            .folder_id
            .filter(|id| *id > 0)
            .map(|folder_id| DragSrc::Folder {
                pin_key: row.pin_key.clone(),
                slug: row.workspace_slug.clone(),
                folder_id,
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
                    // The hovered row's own run is the destination — this is
                    // what makes a drop *inside* a folder file as well as order.
                    let Some(hovered) = row.worktree_path.as_deref() else {
                        return Spot::Invalid;
                    };
                    let Some(folder) =
                        crate::sidebar_order::run_of(&model.sidebar_rows, slug, hovered)
                    else {
                        return Spot::Invalid;
                    };
                    if top_half {
                        Spot::Reorder {
                            before_pin_key: Some(row.pin_key.clone()),
                            folder,
                            viz: DragSpotViz::InsertBefore(hit.visible_index),
                        }
                    } else {
                        // Bottom half: before the next worktree **in the same
                        // run** (not merely the next one on screen — that could
                        // be the first child of the following folder), or at the
                        // end of this run.
                        let next =
                            crate::sidebar_order::next_in_run(&model.sidebar_rows, slug, hovered);
                        let next_row = next.as_deref().and_then(|p| {
                            visible.iter().enumerate().find(|(_, r)| {
                                r.kind == RowKind::Worktree && r.worktree_path.as_deref() == Some(p)
                            })
                        });
                        match next_row {
                            Some((i, r)) => Spot::Reorder {
                                before_pin_key: Some(r.pin_key.clone()),
                                folder,
                                viz: DragSpotViz::InsertBefore(i),
                            },
                            None => Spot::Reorder {
                                before_pin_key: None,
                                folder,
                                viz: DragSpotViz::InsertAfter(hit.visible_index),
                            },
                        }
                    }
                }
                _ => Spot::Invalid,
            }
        }
        DragSrc::Folder {
            slug, folder_id, ..
        } => {
            // Folders reorder among their own workspace's folders. Any row
            // resolves to the folder that encloses it; the workspace header
            // means "first".
            if row.workspace_slug != *slug {
                return Spot::Invalid;
            }
            if row.kind == RowKind::Workspace {
                let first = crate::sidebar_order::folder_order(&model.sidebar_rows, slug)
                    .into_iter()
                    .next();
                if first == Some(*folder_id) {
                    return Spot::Invalid; // already first
                }
                let header = visible
                    .iter()
                    .enumerate()
                    .find(|(_, r)| r.kind == RowKind::Folder && r.folder_id == first);
                return match header {
                    Some((i, r)) => Spot::Reorder {
                        before_pin_key: Some(r.pin_key.clone()),
                        folder: None,
                        viz: DragSpotViz::InsertBefore(i),
                    },
                    None => Spot::Invalid,
                };
            }
            // The folder header owning the hovered row (a depth-2 worktree
            // belongs to the header above it); loose worktrees have none.
            let owner = visible
                .iter()
                .enumerate()
                .take(hit.visible_index + 1)
                .filter(|(_, r)| r.workspace_slug == *slug)
                .fold(None, |acc, (i, r)| match r.kind {
                    RowKind::Folder => Some((i, r)),
                    RowKind::Worktree if r.depth < 2 => None,
                    _ => acc,
                });
            let Some((hi, hrow)) = owner else {
                return Spot::Invalid;
            };
            if hrow.folder_id == Some(*folder_id) {
                return Spot::Invalid; // hovering itself
            }
            // Top half of the header inserts before it; anywhere else in its
            // subtree inserts after — i.e. before the following folder.
            let before = hit.visible_index == hi && my < hit.y + hit.height.div_ceil(2);
            if before {
                Spot::Reorder {
                    before_pin_key: Some(hrow.pin_key.clone()),
                    folder: None,
                    viz: DragSpotViz::InsertBefore(hi),
                }
            } else {
                let next = visible
                    .iter()
                    .enumerate()
                    .skip(hi + 1)
                    .find(|(_, r)| r.kind == RowKind::Folder && r.workspace_slug == *slug);
                match next {
                    Some((i, r)) if r.folder_id != Some(*folder_id) => Spot::Reorder {
                        before_pin_key: Some(r.pin_key.clone()),
                        folder: None,
                        viz: DragSpotViz::InsertBefore(i),
                    },
                    // The next folder IS the source: dropping just above itself
                    // is a no-op, but landing last is not.
                    Some(_) => Spot::Invalid,
                    None => Spot::Reorder {
                        before_pin_key: None,
                        folder: None,
                        viz: DragSpotViz::InsertAfter(hi),
                    },
                }
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
                    folder: None,
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
                        folder: None,
                        viz: DragSpotViz::InsertBefore(i),
                    },
                    None => Spot::Reorder {
                        before_pin_key: None,
                        folder: None,
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
                // Land it at the end of the folder rather than wherever its old
                // position happens to sort it among the folder's members.
                move_to_end_of_run(sb, model, session, path);
            }
        }
        (DragSrc::Worktree { path, .. }, Spot::Unfile { .. }) => {
            model.status = crate::handlers::sidebar_folder::unfile_worktree_path(
                session, sb, model, path, refresh_tx, waker,
            );
            // Land it at the end of the loose run rather than wherever its old
            // position happens to sort it.
            move_to_end_of_run(sb, model, session, path);
        }
        (
            DragSrc::Worktree { path, slug, .. },
            Spot::Reorder {
                before_pin_key,
                folder,
                ..
            },
        ) => {
            // Resolve the insertion anchor's pin key back to a path — the
            // ordering model is keyed on paths, which survive a re-file. An
            // anchor that VANISHED mid-drag (hydration filed/deleted it) must
            // abandon the drop, not collapse `Some(anchor)` into `None` —
            // `drop_at` reads `None` as "append to the end of the run", which
            // would land the row somewhere the user never aimed. (This is the
            // same bail `drop_at` itself performs for a stale in-run anchor.)
            let before = match before_pin_key.as_deref() {
                Some(k) => match path_of_pin_key(model, k) {
                    Some(p) => Some(p),
                    None => return,
                },
                None => None,
            };
            if let Some(plan) = crate::sidebar_order::drop_at(
                &model.sidebar_rows,
                slug,
                path,
                *folder,
                before.as_deref(),
            ) {
                sb.apply_order_plan(model, session, slug, plan);
            }
        }
        (DragSrc::Workspace { slug, .. }, Spot::Reorder { before_pin_key, .. }) => {
            reorder_workspace_to(sb, model, session, slug, before_pin_key.as_deref());
        }
        (
            DragSrc::Folder {
                slug, folder_id, ..
            },
            Spot::Reorder { before_pin_key, .. },
        ) => {
            // Same vanished-anchor bail as the worktree arm above.
            let before = match before_pin_key.as_deref() {
                Some(k) => match folder_id_of_pin_key(model, k) {
                    Some(id) => Some(id),
                    None => return,
                },
                None => None,
            };
            if let Some(order) =
                crate::sidebar_order::drop_folder_at(&model.sidebar_rows, slug, *folder_id, before)
            {
                sb.apply_folder_order(model, session, slug, order);
            }
        }
        // Workspaces and folders can't file/unfile.
        (
            DragSrc::Workspace { .. } | DragSrc::Folder { .. },
            Spot::FileInto { .. } | Spot::Unfile { .. },
        ) => {}
    }
}

/// Move the worktree at `path` to the end of the run it is currently in. Used
/// after a header drop (file / unfile), which changes membership but leaves the
/// row's stale `position` deciding where inside the run it lands.
fn move_to_end_of_run(
    sb: &mut SidebarState,
    model: &mut FrameModel,
    session: &mut crate::session::Session,
    path: &str,
) {
    let Some(slug) = model
        .sidebar_rows
        .iter()
        .find(|r| r.kind == RowKind::Worktree && r.worktree_path.as_deref() == Some(path))
        .map(|r| r.workspace_slug.clone())
    else {
        return;
    };
    let Some(folder) = crate::sidebar_order::run_of(&model.sidebar_rows, &slug, path) else {
        return;
    };
    if let Some(plan) =
        crate::sidebar_order::drop_at(&model.sidebar_rows, &slug, path, folder, None)
    {
        sb.apply_order_plan(model, session, &slug, plan);
    }
}

/// The worktree path behind a worktree row's `pin_key`.
fn path_of_pin_key(model: &FrameModel, key: &str) -> Option<String> {
    model
        .sidebar_rows
        .iter()
        .find(|r| r.kind == RowKind::Worktree && r.pin_key == key)
        .and_then(|r| r.worktree_path.clone())
}

/// The folder id behind a folder row's `pin_key`.
fn folder_id_of_pin_key(model: &FrameModel, key: &str) -> Option<i64> {
    model
        .sidebar_rows
        .iter()
        .find(|r| r.kind == RowKind::Folder && r.pin_key == key)
        .and_then(|r| r.folder_id)
}

/// Step-move the source workspace to the target slot (the worktree counterpart
/// is `move_worktree_path`; workspaces move via `move_workspace_by_slug`).
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

    // --- folder-aware drop resolution ------------------------------------

    fn filed_row(slug: &str, branch: &str, fid: i64, gi: usize) -> SidebarRow {
        SidebarRow {
            tab_target: Some(RowTarget::Tab(gi, 0)),
            worktree_path: Some(format!("/wt/{branch}")),
            pin_key: format!("{slug}/{branch}/folder:{fid}"),
            branch: Some(branch.into()),
            ..SidebarRow::base(RowKind::Worktree, 2, branch, slug)
        }
    }

    /// app: home, feat loose; folder 1 "Backend" { api, db }; folder 2
    /// "Frontend" { web }.
    fn folder_fixture() -> (crate::chrome::FrameModel, crate::compositor::Rect) {
        let model = crate::chrome::FrameModel {
            sidebar_rows: vec![
                ws_row("app"),
                wt_row("app", "home", 0),
                wt_row("app", "feat", 1),
                folder_row("app", 1, "Backend"),
                filed_row("app", "api", 1, 2),
                filed_row("app", "db", 1, 3),
                folder_row("app", 2, "Frontend"),
                filed_row("app", "web", 2, 4),
            ],
            sidebar_workspaces: vec![(
                "app".into(),
                "app".into(),
                "repo".into(),
                "/repos/app".into(),
            )],
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

    #[test]
    fn dropping_inside_a_folder_targets_that_folders_run() {
        let (model, rect) = folder_fixture();
        let sb = SidebarState::default();
        let session = crate::session::Session::default();
        // Top half of `db`, which lives in folder 1 → insert before it, in
        // folder 1. Without the `folder` field this resolved to a flat
        // workspace-wide reorder and the renderer put the row straight back.
        let y = y_of(&model, rect, "app/db/folder:1");
        match spot_at(&sb, &model, &session, rect, &src_feat(), y) {
            Spot::Reorder {
                before_pin_key,
                folder,
                ..
            } => {
                assert_eq!(before_pin_key.as_deref(), Some("app/db/folder:1"));
                assert_eq!(folder, Some(1));
            }
            other => panic!("expected a folder-targeted reorder, got {other:?}"),
        }
    }

    #[test]
    fn a_bottom_half_drop_stops_at_the_run_boundary() {
        let (mut model, rect) = folder_fixture();
        // Only the cursor row of a focused sidebar renders a detail line, and
        // only a >1-line row *has* a bottom half to aim at.
        model.sidebar_focused = true;
        model.sidebar_selected = 5; // db
        let sb = SidebarState::default();
        let session = crate::session::Session::default();
        // Bottom half of `db` — the last member of folder 1. The next row on
        // screen is folder 2's header, but the insertion must stay in folder 1
        // and land at the end of its run.
        let y = y_of(&model, rect, "app/db/folder:1") + 1;
        match spot_at(&sb, &model, &session, rect, &src_feat(), y) {
            Spot::Reorder {
                before_pin_key,
                folder,
                ..
            } => {
                assert_eq!(folder, Some(1));
                assert_eq!(
                    before_pin_key, None,
                    "must not spill into the following folder's run"
                );
            }
            other => panic!("expected reorder at the end of folder 1, got {other:?}"),
        }
    }

    #[test]
    fn a_bottom_half_drop_in_the_loose_run_stays_loose() {
        let (mut model, rect) = folder_fixture();
        model.sidebar_focused = true;
        model.sidebar_selected = 2; // feat
        let sb = SidebarState::default();
        let session = crate::session::Session::default();
        let src = DragSrc::Worktree {
            pin_key: "app/api/folder:1".into(),
            slug: "app".into(),
            path: "/wt/api".into(),
        };
        // Bottom half of `feat`, the last loose worktree: land at the end of
        // the loose run — which unfiles the dragged worktree.
        let y = y_of(&model, rect, "app/feat") + 1;
        match spot_at(&sb, &model, &session, rect, &src, y) {
            Spot::Reorder {
                before_pin_key,
                folder,
                ..
            } => {
                assert_eq!(folder, None);
                assert_eq!(before_pin_key, None);
            }
            other => panic!("expected a loose reorder, got {other:?}"),
        }
    }

    #[test]
    fn a_folder_header_drags_among_its_workspaces_folders() {
        let (model, rect) = folder_fixture();
        let sb = SidebarState::default();
        let session = crate::session::Session::default();
        let src = DragSrc::Folder {
            pin_key: "app/folder:2".into(),
            slug: "app".into(),
            folder_id: 2,
        };
        // Top half of folder 1's header → insert before it.
        let y = y_of(&model, rect, "app/folder:1");
        match spot_at(&sb, &model, &session, rect, &src, y) {
            Spot::Reorder { before_pin_key, .. } => {
                assert_eq!(before_pin_key.as_deref(), Some("app/folder:1"));
            }
            other => panic!("expected a folder reorder, got {other:?}"),
        }
        // Hovering its own subtree is a no-op.
        let y = y_of(&model, rect, "app/web/folder:2");
        assert_eq!(spot_at(&sb, &model, &session, rect, &src, y), Spot::Invalid);
        // A loose worktree encloses no folder → nothing to order against.
        let y = y_of(&model, rect, "app/feat");
        assert_eq!(spot_at(&sb, &model, &session, rect, &src, y), Spot::Invalid);
    }

    #[test]
    fn a_synthetic_folder_is_not_draggable_until_its_row_exists() {
        let (model, rect) = folder_fixture();
        let sb = SidebarState::default();
        let session = crate::session::Session::default();
        let hits = hit_rows(&model, rect);
        let visible: Vec<&SidebarRow> = model.sidebar_rows.iter().filter(|r| r.visible).collect();

        // A real folder header is a drag source…
        let hit = hits
            .iter()
            .find(|h| visible[h.visible_index].pin_key == "app/folder:1")
            .unwrap();
        assert!(matches!(
            drag_src_for(&sb, &model, &session, hit),
            Some(DragSrc::Folder { folder_id: 1, .. })
        ));

        // …but an optimistically-created one (synthetic negative id, no DB row)
        // has no position to renumber yet.
        let mut model2 = model.clone();
        model2.sidebar_rows[3].folder_id = Some(-1);
        assert!(drag_src_for(&sb, &model2, &session, hit).is_none());
    }
}
