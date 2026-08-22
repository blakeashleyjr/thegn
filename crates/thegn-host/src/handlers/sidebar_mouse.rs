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
    /// Land the source in the slot currently held by the row `slot_pin_key`
    /// (displacement) — the row it displaces shifts one step toward where the
    /// source came from.
    ///
    /// Naming a **stable row identity** rather than a resolved index is what
    /// lets a mid-drag hydration be *validated* at release (bail if the row is
    /// gone) instead of silently reinterpreted as some other slot. And there is
    /// deliberately no "at the tail" variant: under displacement the tail is
    /// just the last row's slot, so every reorder names a row. Only the header
    /// affordances mean "land last", and they say so with
    /// [`crate::sidebar_order::Landing::Tail`].
    ///
    /// `folder` is the **destination run** for a worktree drag — `None` for the
    /// loose list, `Some(id)` for a folder. Carrying it here is what lets a drop
    /// *inside a folder* both file and position the worktree; without it the
    /// drop reordered a flat workspace-wide run and the renderer put the row
    /// straight back where it came from.
    Reorder {
        slot_pin_key: String,
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
    /// the pressed row's band (sub-row jitter stays a click). The band is
    /// re-derived from the row's live placement on each sample rather than
    /// captured here — captured coordinates go stale as soon as focus reflows
    /// the rows.
    Pressed {
        src: DragSrc,
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
    /// Row heights as the pressed frame painted them, held for the gesture.
    lock: Option<crate::sidebar_view::SidebarLayoutLock>,
}

impl MouseUi {
    /// The layout freeze the renderer should honour this frame, if any.
    ///
    /// Gated on the phase rather than on `lock` alone, so a missed clear can
    /// never wedge the sidebar's geometry: no gesture, no freeze.
    pub(crate) fn layout_lock(&self) -> Option<crate::sidebar_view::SidebarLayoutLock> {
        match self.drag {
            DragPhase::Idle => None,
            _ => self.lock.clone(),
        }
    }

    /// Whether a drag gesture is armed or in flight. While this holds, the
    /// pointer is CAPTURED by the sidebar: mouse events must not be forwarded
    /// to (or swallowed by) a mouse-reporting pane, or the release never
    /// arrives and the gesture can never end.
    pub(crate) fn drag_active(&self) -> bool {
        self.drag != DragPhase::Idle
    }
}

/// Abandon an in-flight gesture without reordering anything (Escape, or any
/// other interruption). Returns whether a gesture was actually live.
pub(crate) fn cancel_drag(ui: &mut MouseUi, model: &mut FrameModel) -> bool {
    let was = ui.drag_active();
    ui.drag = DragPhase::Idle;
    ui.lock = None;
    model.sidebar_drag = None;
    model.sidebar_drag_lock = None;
    was
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
    // Capture the geometry the pressed frame was PAINTED with, before the
    // cursor move below can change the detail tier, and hold it for the whole
    // gesture (see `SidebarLayoutLock`).
    let painted = crate::sidebar_view::SidebarLayoutLock {
        detail_focused: model.sidebar_focused,
        detail_cursor: model.sidebar_selected,
    };
    ui.drag = drag_src_for(sb, model, session, &hit)
        .map(|src| DragPhase::Pressed { src })
        .unwrap_or(DragPhase::Idle);
    ui.lock = match ui.drag {
        DragPhase::Idle => None,
        _ => Some(painted),
    };

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
    rect: crate::compositor::Rect,
    my: usize,
) -> bool {
    match std::mem::take(&mut ui.drag) {
        DragPhase::Idle => {
            ui.drag = DragPhase::Idle;
            false
        }
        DragPhase::Pressed { src } => {
            // Re-derive the pressed row's band from its LIVE placement rather
            // than from coordinates captured at press time: rows reflow when
            // the sidebar gains or loses focus (the detail tier), so a captured
            // band goes stale the moment the gesture starts.
            let hits = hit_rows(model, rect);
            match hits.iter().find(|h| h.pin_key == src.pin_key()) {
                Some(h) if my >= h.y && my < h.y + h.height => {
                    // Still inside the pressed row: sub-row jitter is a click.
                    ui.drag = DragPhase::Pressed { src };
                    true
                }
                Some(_) => {
                    let spot = spot_at(model, rect, &src, my);
                    apply_viz(model, rect, &src, &spot);
                    ui.drag = DragPhase::Dragging { src, spot };
                    true
                }
                // The source row is gone (hydration pruned it): cancel rather
                // than drag a row that no longer exists.
                None => {
                    ui.drag = DragPhase::Idle;
                    false
                }
            }
        }
        DragPhase::Dragging { src, .. } => {
            let step = autoscroll_step(my, rect);
            if step != 0 {
                let visible = SidebarState::visible_len(model);
                if visible > 0 {
                    let last = visible - 1;
                    sb.cursor = sb.cursor.saturating_add_signed(step).min(last);
                    sb.sync(model);
                }
            }
            // Resolve the spot AFTER any scroll so it reflects the geometry the
            // next frame paints.
            let spot = spot_at(model, rect, &src, my);
            apply_viz(model, rect, &src, &spot);
            ui.drag = DragPhase::Dragging { src, spot };
            true
        }
    }
}

/// How far to walk the cursor for a drag sample at screen row `my` — 0 inside
/// the list, and otherwise **proportional to the overshoot** past the edge
/// (capped, mirroring the pane-selection drag in `run.rs`).
///
/// Proportionality is not cosmetic. Mouse mode 1002 only reports motion while a
/// button is held *and the cell changes*, and `run.rs`'s `drain_drag_events`
/// coalesces a burst of samples down to the last one — so a constant one-row
/// step could never keep up with a flick to the edge, and a pointer parked past
/// the edge would scroll by one row and stop. (No timer: an idle loop must never
/// poll, so a genuinely stationary pointer still emits nothing.)
pub(crate) fn autoscroll_step(my: usize, rect: crate::compositor::Rect) -> isize {
    /// Rows of the list that count as the "pull" band at each edge.
    const BAND: usize = 2;
    /// Never jump more than this per sample, however far past the edge.
    const MAX: usize = 3;
    let top = rect.y + BAND;
    let bottom = rect.y + rect.rows.saturating_sub(1 + BAND);
    if my < top {
        -((top - my).min(MAX) as isize)
    } else if my > bottom {
        (my - bottom).min(MAX) as isize
    } else {
        0
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
    ui.lock = None;
    model.sidebar_drag = None;
    model.sidebar_drag_lock = None;
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
        .map(|h| h.visible_index);
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

/// Resolve the pointer's drop spot for `src` — the thin geometry adapter over
/// [`spot_for_hover`], which owns the actual rule.
fn spot_at(model: &FrameModel, rect: crate::compositor::Rect, src: &DragSrc, my: usize) -> Spot {
    let hits = hit_rows(model, rect);
    match row_at(&hits, my) {
        Some(hit) => spot_for_hover(&model.sidebar_rows, hit.visible_index, src),
        None => Spot::Invalid,
    }
}

/// The drop rule: where a release over visible row `hovered` would land `src`.
///
/// Pure over the row tree — no rect, no hit geometry, no `SidebarState`, no
/// `Session`, and crucially **no y-coordinate**. A sidebar row is one terminal
/// cell tall, so there is no sub-row resolution to base a "top half / bottom
/// half" on: the hovered row's SLOT is the destination, and the row it displaces
/// shifts one step toward where the source came from (see
/// [`crate::sidebar_order::place_at`]).
///
/// The previous rule split each row at `hit.height.div_ceil(2)`, which for a
/// 1-cell row is always the top half — so every drop meant "insert before the
/// hovered row" (one slot above the aim point) and the end of a run had no
/// anchor to name at all.
pub(crate) fn spot_for_hover(
    rows: &[crate::sidebar::SidebarRow],
    hovered: usize,
    src: &DragSrc,
) -> Spot {
    let visible: Vec<&crate::sidebar::SidebarRow> = rows.iter().filter(|r| r.visible).collect();
    let Some(row) = visible.get(hovered).copied() else {
        return Spot::Invalid;
    };
    if row.pin_key == src.pin_key() {
        return Spot::Invalid; // hovering the source itself
    }
    match src {
        DragSrc::Worktree { slug, path, .. } => {
            if row.workspace_slug != *slug {
                return Spot::Invalid; // worktrees never cross workspaces
            }
            match row.kind {
                RowKind::Folder => Spot::FileInto {
                    folder_name: row.label.clone(),
                    viz_index: hovered,
                },
                RowKind::Workspace => Spot::Unfile { viz_index: hovered },
                RowKind::Worktree => {
                    let Some(anchor) = row.worktree_path.as_deref() else {
                        return Spot::Invalid;
                    };
                    // One `runs()` pass serves both lookups (this runs on every
                    // motion sample; the old code called it twice).
                    let runs = crate::sidebar_order::runs(rows, slug);
                    let Some((dest_ri, h)) = crate::sidebar_order::locate(&runs, anchor) else {
                        return Spot::Invalid;
                    };
                    // Home is anchored at the head of the loose run: its slot is
                    // not a destination, whatever part of the row you are over.
                    if h == 0 && runs[dest_ri].members[0].home {
                        return Spot::Invalid;
                    }
                    // The hovered row's own run is the destination — this is what
                    // makes a drop *inside* a folder file as well as order.
                    let folder = runs[dest_ri].folder;
                    // Direction is needed only for the RULE'S FEEDBACK: dragging
                    // down, the source lands after the hovered row, so the rule
                    // paints below it.
                    let below = crate::sidebar_order::locate(&runs, path)
                        .is_some_and(|(ri, s)| ri == dest_ri && s < h);
                    let viz = if below {
                        DragSpotViz::InsertAfter(crate::sidebar_order::block_end(rows, hovered))
                    } else {
                        DragSpotViz::InsertBefore(hovered)
                    };
                    Spot::Reorder {
                        slot_pin_key: row.pin_key.clone(),
                        folder,
                        viz,
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
            // means "the first folder".
            if row.workspace_slug != *slug {
                return Spot::Invalid;
            }
            let order = crate::sidebar_order::folder_order(rows, slug);
            let anchor = if row.kind == RowKind::Workspace {
                order.first().copied()
            } else {
                // The folder header owning the hovered row (a depth-2 worktree
                // belongs to the header above it); loose worktrees have none.
                visible
                    .iter()
                    .enumerate()
                    .take(hovered + 1)
                    .filter(|(_, r)| r.workspace_slug == *slug)
                    .fold(None, |acc, (_, r)| match r.kind {
                        RowKind::Folder => r.folder_id,
                        RowKind::Worktree if r.depth < 2 => None,
                        _ => acc,
                    })
            };
            let Some(anchor) = anchor.filter(|a| a != folder_id) else {
                return Spot::Invalid; // no enclosing folder, or hovering itself
            };
            let (Some(hi), Some(si)) = (
                visible
                    .iter()
                    .position(|r| r.kind == RowKind::Folder && r.folder_id == Some(anchor)),
                order.iter().position(|f| f == folder_id),
            ) else {
                return Spot::Invalid;
            };
            let below = order
                .iter()
                .position(|f| *f == anchor)
                .is_some_and(|h| si < h);
            Spot::Reorder {
                slot_pin_key: visible[hi].pin_key.clone(),
                folder: None,
                viz: if below {
                    DragSpotViz::InsertAfter(crate::sidebar_order::block_end(rows, hi))
                } else {
                    DragSpotViz::InsertBefore(hi)
                },
            }
        }
        DragSrc::Workspace { slug, .. } => {
            // Any row resolves to its enclosing workspace; the terminals region
            // is out of bounds.
            if row.workspace_slug == *slug {
                return Spot::Invalid;
            }
            if row.workspace_slug == "terminals" || row.workspace_slug.starts_with("terminals/") {
                return Spot::Invalid;
            }
            let order = crate::sidebar_order::workspace_order(rows);
            let anchor = &row.workspace_slug;
            let (Some(hi), Some(h), Some(si)) = (
                visible
                    .iter()
                    .position(|r| r.kind == RowKind::Workspace && r.workspace_slug == *anchor),
                order.iter().position(|s| s == anchor),
                order.iter().position(|s| s == slug),
            ) else {
                return Spot::Invalid;
            };
            Spot::Reorder {
                slot_pin_key: visible[hi].pin_key.clone(),
                folder: None,
                viz: if si < h {
                    DragSpotViz::InsertAfter(crate::sidebar_order::block_end(rows, hi))
                } else {
                    DragSpotViz::InsertBefore(hi)
                },
            }
        }
    }
}

/// A drop resolved to the new order it implies, before anything is applied.
///
/// Split out of [`perform_drop`] so the ordering half of a drop is testable:
/// the `Spot::Reorder` arms need neither a `TerminalWaker` nor a refresh
/// channel (`apply_order_plan` / `apply_folder_order` take only
/// `(model, session)`), but `TerminalWaker` has a private field and no public
/// constructor, so anything that demands one cannot be unit-tested at all.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Resolved {
    /// The workspace's full new worktree order (+ any re-file).
    Worktree(crate::sidebar_order::Plan),
    /// The workspace's new folder id order.
    Folders(Vec<i64>),
    /// The new workspace slug order.
    Workspaces(Vec<String>),
}

/// What `spot` would do to the order, or `None` when the drop is a no-op or
/// its anchor vanished mid-drag. Pure: reads the model, changes nothing.
pub(crate) fn resolve_reorder(model: &FrameModel, src: &DragSrc, spot: &Spot) -> Option<Resolved> {
    let Spot::Reorder {
        slot_pin_key,
        folder,
        ..
    } = spot
    else {
        return None;
    };
    match src {
        DragSrc::Worktree { path, slug, .. } => {
            // The ordering model is path-keyed (paths survive a re-file); the
            // spot is pin_key-keyed (that is the hit/viz identity). Resolve one
            // to the other, and BAIL if the anchor vanished — landing the row
            // "somewhere reasonable" instead would put it where the user never
            // aimed.
            let anchor = path_of_pin_key(model, slot_pin_key)?;
            crate::sidebar_order::place_at(
                &model.sidebar_rows,
                slug,
                path,
                *folder,
                crate::sidebar_order::Landing::Slot(&anchor),
            )
            .map(Resolved::Worktree)
        }
        DragSrc::Folder {
            slug, folder_id, ..
        } => {
            // Same vanished-anchor bail as the worktree arm.
            let anchor = folder_id_of_pin_key(model, slot_pin_key)?;
            crate::sidebar_order::displace_folder(&model.sidebar_rows, slug, *folder_id, anchor)
                .map(Resolved::Folders)
        }
        DragSrc::Workspace { slug, .. } => {
            let anchor = slug_of_pin_key(model, slot_pin_key)?;
            crate::sidebar_order::displace_workspace(&model.sidebar_rows, slug, &anchor)
                .map(Resolved::Workspaces)
        }
    }
}

/// Apply a resolved reorder drop. The waker-free half of [`perform_drop`].
pub(crate) fn apply_reorder_drop(
    sb: &mut SidebarState,
    model: &mut FrameModel,
    session: &mut crate::session::Session,
    src: &DragSrc,
    spot: &Spot,
) -> bool {
    match resolve_reorder(model, src, spot) {
        Some(Resolved::Worktree(plan)) => {
            let DragSrc::Worktree { slug, .. } = src else {
                return false;
            };
            sb.apply_order_plan(model, session, slug, plan)
        }
        Some(Resolved::Folders(order)) => {
            let DragSrc::Folder { slug, .. } = src else {
                return false;
            };
            sb.apply_folder_order(model, session, slug, order)
        }
        Some(Resolved::Workspaces(order)) => sb.apply_workspace_order(model, session, order),
        None => false,
    }
}

/// The workspace slug behind a workspace header row's `pin_key`.
fn slug_of_pin_key(model: &FrameModel, key: &str) -> Option<String> {
    model
        .sidebar_rows
        .iter()
        .find(|r| r.kind == RowKind::Workspace && r.pin_key == key)
        .map(|r| r.workspace_slug.clone())
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
        // Every reorder owns no I/O — it goes through the waker-free seam, so
        // the same code path is unit-testable and every drop is ONE resolved
        // order (the workspace axis used to step-walk, and could bail out
        // mid-way leaving the workspace parked between source and target).
        (_, Spot::Reorder { .. }) => {
            apply_reorder_drop(sb, model, session, src, spot);
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
    if let Some(plan) = crate::sidebar_order::place_at(
        &model.sidebar_rows,
        &slug,
        path,
        folder,
        crate::sidebar_order::Landing::Tail,
    ) {
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

    /// Convenience: resolve a hover by pin key rather than by screen row.
    fn spot_on(
        model: &crate::chrome::FrameModel,
        rect: crate::compositor::Rect,
        src: &DragSrc,
        pin: &str,
    ) -> Spot {
        spot_at(model, rect, src, y_of(model, rect, pin))
    }

    #[test]
    fn spot_worktree_never_crosses_workspaces_and_never_displaces_home() {
        let (model, rect) = fixture();
        // Another workspace's worktree.
        assert_eq!(
            spot_on(&model, rect, &src_feat(), "lib/home"),
            Spot::Invalid,
            "worktrees never cross workspaces"
        );
        // `home` is anchored at the head of the loose run, so its slot is not a
        // destination — whatever part of the row you are over. (There is no
        // "part of the row": a row is one terminal cell.)
        assert_eq!(
            spot_on(&model, rect, &src_feat(), "app/home"),
            Spot::Invalid,
            "nothing may displace home"
        );
    }

    #[test]
    fn spot_worktree_files_into_folder_and_unfiles_on_workspace_header() {
        let (model, rect) = fixture();
        match spot_on(&model, rect, &src_feat(), "app/folder:1") {
            Spot::FileInto { folder_name, .. } => assert_eq!(folder_name, "Backend"),
            other => panic!("expected FileInto, got {other:?}"),
        }
        assert!(matches!(
            spot_on(&model, rect, &src_feat(), "app"),
            Spot::Unfile { .. }
        ));
    }

    #[test]
    fn spot_workspace_displaces_the_hovered_workspace() {
        let (model, rect) = fixture();
        let src = DragSrc::Workspace {
            pin_key: "lib".into(),
            slug: "lib".into(),
        };
        // Hovering anywhere in app's subtree resolves to app's header, and `lib`
        // takes app's slot. This used to be a DEAD ZONE: the old rule resolved
        // app's subtree to "insert before lib" — i.e. before itself — so the
        // executor no-oped over app's entire subtree.
        for pin in ["app", "app/feat"] {
            match spot_on(&model, rect, &src, pin) {
                Spot::Reorder { slot_pin_key, .. } => assert_eq!(slot_pin_key, "app"),
                other => panic!("expected a workspace reorder over {pin}, got {other:?}"),
            }
        }
        assert_eq!(
            resolve_reorder(&model, &src, &spot_on(&model, rect, &src, "app/feat")),
            Some(Resolved::Workspaces(vec!["lib".into(), "app".into()])),
        );
        // Its own subtree is not a destination.
        assert_eq!(spot_on(&model, rect, &src, "lib/home"), Spot::Invalid);
    }

    #[test]
    fn pressed_becomes_dragging_only_after_leaving_the_row_band() {
        let (mut model, rect) = fixture();
        let mut sb = SidebarState::default();
        let mut ui = MouseUi {
            drag: DragPhase::Pressed { src: src_feat() },
            ..Default::default()
        };
        let y = y_of(&model, rect, "app/feat");
        // Sample inside the pressed row: still a (potential) click.
        assert!(on_drag_move(&mut ui, &mut sb, &mut model, rect, y));
        assert!(matches!(ui.drag, DragPhase::Pressed { .. }));
        assert!(model.sidebar_drag.is_none());
        // Leaving it starts the drag and mirrors the viz on the model.
        let y2 = y_of(&model, rect, "app/zeta");
        assert!(on_drag_move(&mut ui, &mut sb, &mut model, rect, y2));
        assert!(matches!(ui.drag, DragPhase::Dragging { .. }));
        assert!(model.sidebar_drag.is_some());
    }

    #[test]
    fn release_after_plain_press_is_a_click_and_clears_state() {
        let (mut model, _rect) = fixture();
        let mut ui = MouseUi {
            drag: DragPhase::Pressed { src: src_feat() },
            ..Default::default()
        };
        assert_eq!(on_release(&mut ui, &mut model), ReleaseOut::Click);
        assert_eq!(ui.drag, DragPhase::Idle);
        assert!(model.sidebar_drag.is_none());
        assert!(model.sidebar_drag_lock.is_none());
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

    #[test]
    fn escape_cancels_an_in_flight_drag_without_reordering() {
        let (mut model, rect) = fixture();
        let mut sb = SidebarState::default();
        let mut ui = MouseUi {
            drag: DragPhase::Pressed { src: src_feat() },
            lock: Some(crate::sidebar_view::SidebarLayoutLock {
                detail_focused: false,
                detail_cursor: 0,
            }),
            ..Default::default()
        };
        let y = y_of(&model, rect, "app/zeta");
        on_drag_move(&mut ui, &mut sb, &mut model, rect, y);
        assert!(ui.drag_active());

        assert!(
            cancel_drag(&mut ui, &mut model),
            "a live gesture was cancelled"
        );
        assert_eq!(ui.drag, DragPhase::Idle);
        assert!(model.sidebar_drag.is_none());
        assert!(model.sidebar_drag_lock.is_none());
        assert!(ui.layout_lock().is_none());
        // Cancelling again is a no-op, and a stale gesture must never hijack the
        // next left-drag anywhere in the app.
        assert!(!cancel_drag(&mut ui, &mut model));
        assert!(!on_drag_move(&mut ui, &mut sb, &mut model, rect, y));
        // …and a late release is inert.
        assert_eq!(on_release(&mut ui, &mut model), ReleaseOut::NotOurs);
    }

    #[test]
    fn a_live_drag_captures_the_pointer_away_from_a_reporting_pane() {
        use crate::handlers::overlay::should_forward_to_pane;
        // Without a drag, a mouse-reporting pane owns the pointer unless Shift
        // is held (the convention every terminal uses).
        assert!(should_forward_to_pane(true, false, false));
        assert!(!should_forward_to_pane(true, true, false));
        assert!(!should_forward_to_pane(false, false, false));
        // With a drag in flight the sidebar owns it, whatever the pane wants —
        // otherwise the RELEASE is written into the pane and consumed, the
        // gesture never ends, and the next left-drag anywhere is hijacked.
        for shift in [false, true] {
            for reports in [false, true] {
                assert!(
                    !should_forward_to_pane(reports, shift, true),
                    "a live drag must capture the pointer (reports={reports}, shift={shift})"
                );
            }
        }
    }

    #[test]
    fn edge_autoscroll_step_scales_with_overshoot() {
        let rect = crate::compositor::Rect {
            x: 0,
            y: 10,
            cols: 30,
            rows: 20,
        };
        // The list spans rows 10..=29, so the pull bands are 10..=11 and 28..=29.
        for my in 12..=27 {
            assert_eq!(autoscroll_step(my, rect), 0, "row {my} is inside the band");
        }
        // Past an edge: proportional and signed, so a coalesced burst (see
        // `drain_drag_events`) still travels the distance the pointer did.
        assert_eq!(autoscroll_step(11, rect), -1);
        assert_eq!(autoscroll_step(10, rect), -2);
        assert_eq!(autoscroll_step(28, rect), 1);
        assert_eq!(autoscroll_step(29, rect), 2);
        // …and capped, so one wild sample cannot fling the list.
        assert_eq!(autoscroll_step(0, rect), -3);
        assert_eq!(autoscroll_step(999, rect), 3);
        // Monotone in each direction.
        let down: Vec<isize> = (28..40).map(|my| autoscroll_step(my, rect)).collect();
        assert!(down.windows(2).all(|w| w[0] <= w[1]));
        let up: Vec<isize> = (0..12).map(|my| autoscroll_step(my, rect)).collect();
        assert!(up.windows(2).all(|w| w[0] <= w[1]));
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
        // Hovering `db`, which lives in folder 1, targets FOLDER 1's run — not a
        // flat workspace-wide reorder, which the renderer would undo on the next
        // rebuild by re-partitioning by folder.
        match spot_on(&model, rect, &src_feat(), "app/db/folder:1") {
            Spot::Reorder {
                slot_pin_key,
                folder,
                ..
            } => {
                assert_eq!(slot_pin_key, "app/db/folder:1");
                assert_eq!(folder, Some(1));
            }
            other => panic!("expected a folder-targeted reorder, got {other:?}"),
        }
    }

    /// A drop at the end of one run must not spill into the next one on screen.
    ///
    /// This replaces `a_bottom_half_drop_stops_at_the_run_boundary`, which set
    /// `sidebar_focused = true; sidebar_selected = 5` purely to inflate a row to
    /// two lines so a "bottom half" existed to aim at — a geometry the resting
    /// sidebar never has. The invariant it was guarding is real, so it is kept
    /// here and reached the honest way: at any row height, hovering the last
    /// member of folder 1 lands at folder 1's end.
    #[test]
    fn a_drop_at_the_end_of_a_run_stays_inside_that_run() {
        let (model, rect) = folder_fixture();
        // `api` and `db` are folder 1; folder 2's header is the next row after
        // `db` on screen, and `web` is folder 2's only member.
        let src = wt_src("app", "app/api/folder:1", "/wt/api");
        let spot = spot_on(&model, rect, &src, "app/db/folder:1");
        match &spot {
            Spot::Reorder { folder, .. } => assert_eq!(*folder, Some(1)),
            other => panic!("expected a reorder inside folder 1, got {other:?}"),
        }
        let Some(Resolved::Worktree(plan)) = resolve_reorder(&model, &src, &spot) else {
            panic!("expected a resolved plan");
        };
        // api lands after db — the END of folder 1 — and folder 2 is untouched.
        assert_eq!(
            plan.order,
            vec![
                "/wt/home".to_string(),
                "/wt/feat".into(),
                "/wt/db".into(),
                "/wt/api".into(),
                "/wt/web".into(),
            ],
            "must not spill into the following folder's run"
        );
        assert_eq!(plan.refile, None, "it never left folder 1");
    }

    /// The cross-run counterpart, likewise freed of the inflated-row fixture.
    #[test]
    fn a_drop_into_the_loose_run_unfiles_and_takes_the_hovered_slot() {
        let (model, rect) = folder_fixture();
        let src = wt_src("app", "app/api/folder:1", "/wt/api");
        let spot = spot_on(&model, rect, &src, "app/feat");
        match &spot {
            Spot::Reorder { folder, .. } => {
                assert_eq!(*folder, None, "destination is the loose run")
            }
            other => panic!("expected a loose reorder, got {other:?}"),
        }
        let Some(Resolved::Worktree(plan)) = resolve_reorder(&model, &src, &spot) else {
            panic!("expected a resolved plan");
        };
        assert_eq!(plan.refile, Some(None), "landing loose unfiles it");
        assert_eq!(
            plan.order,
            vec![
                "/wt/home".to_string(),
                "/wt/api".into(),
                "/wt/feat".into(),
                "/wt/db".into(),
                "/wt/web".into(),
            ]
        );
        // The tail of a run you are NOT in stays reachable through the header
        // affordance, which means "file here and land last".
        assert!(matches!(
            spot_on(&model, rect, &src, "app"),
            Spot::Unfile { .. }
        ));
    }

    #[test]
    fn a_folder_header_drags_among_its_workspaces_folders() {
        let (model, rect) = folder_fixture();
        let src = DragSrc::Folder {
            pin_key: "app/folder:2".into(),
            slug: "app".into(),
            folder_id: 2,
        };
        // Folder 2 onto folder 1's header takes folder 1's slot.
        match spot_on(&model, rect, &src, "app/folder:1") {
            Spot::Reorder { slot_pin_key, .. } => assert_eq!(slot_pin_key, "app/folder:1"),
            other => panic!("expected a folder reorder, got {other:?}"),
        }
        assert_eq!(
            resolve_reorder(&model, &src, &spot_on(&model, rect, &src, "app/folder:1")),
            Some(Resolved::Folders(vec![2, 1]))
        );
        // Hovering its own subtree is a no-op.
        assert_eq!(
            spot_on(&model, rect, &src, "app/web/folder:2"),
            Spot::Invalid
        );
        // A loose worktree encloses no folder → nothing to order against.
        assert_eq!(spot_on(&model, rect, &src, "app/feat"), Spot::Invalid);
    }

    /// Folder headers are ALWAYS one cell tall, so the old half-row rule could
    /// never resolve "after the last folder" — the bottom-half branch was dead
    /// code and a folder could not be moved to the end at all.
    #[test]
    fn a_folder_can_be_dragged_to_the_last_slot() {
        let (model, rect) = folder_fixture();
        let src = DragSrc::Folder {
            pin_key: "app/folder:1".into(),
            slug: "app".into(),
            folder_id: 1,
        };
        // Folder 1 onto folder 2 (the last one) — or anywhere in its subtree.
        for pin in ["app/folder:2", "app/web/folder:2"] {
            assert_eq!(
                resolve_reorder(&model, &src, &spot_on(&model, rect, &src, pin)),
                Some(Resolved::Folders(vec![2, 1])),
                "hovering {pin} must move folder 1 last"
            );
        }
    }

    // --- keystone: the row-slot (displacement) contract -------------------
    //
    // Releasing over member row X of run R lands the source at the index X
    // occupied BEFORE the drop, inside R. That rule is the only one that is
    // total when a row is one terminal cell tall (there is no sub-cell
    // resolution to aim a "top half / bottom half" with), and it is what makes
    // every slot — including the tail — reachable.
    //
    // These tests assert the USER-VISIBLE outcome (the resulting order), never
    // the internal mechanic, so they survive any implementation that honours
    // the contract.

    /// Row height in the sidebar is a function of (rail, kind, focused,
    /// `FocusDetail`, detail content, window clipping) — never a constant. A
    /// drop rule that only works at one height is broken for everyone else, so
    /// every drop test runs over this matrix.
    ///
    /// The old `a_bottom_half_drop_*` tests set `sidebar_focused = true` purely
    /// to manufacture a 2-line row, with a comment claiming only the cursor row
    /// expands — which is false (the shipped default is `FocusDetail::All`, so
    /// EVERY branch-bearing worktree row expands). Pinning one cell of this
    /// matrix and calling it the world is how the bug survived its own tests.
    #[derive(Clone, Copy, Debug)]
    struct Geom {
        focused: bool,
        detail: thegn_core::config::FocusDetail,
    }

    const GEOMS: &[Geom] = {
        use thegn_core::config::FocusDetail as F;
        &[
            // Resting state: sidebar unfocused ⇒ every row is 1 cell. This is
            // where a drag STARTS when you were typing in a center pane, because
            // activating a row hands focus back to the center (run.rs:11970).
            Geom {
                focused: false,
                detail: F::All,
            },
            Geom {
                focused: true,
                detail: F::Off,
            },
            Geom {
                focused: true,
                detail: F::Cursor,
            },
            // The shipped default.
            Geom {
                focused: true,
                detail: F::All,
            },
        ]
    };

    /// Apply a geometry cell to the model. Goes through the same fields
    /// `SidebarState::sync` writes, never a bespoke height override.
    fn with_geom(model: &mut crate::chrome::FrameModel, g: Geom) {
        model.sidebar_focused = g.focused;
        model.sidebar_display.focus_detail = g.detail;
    }

    /// app: home, a, b, c loose; folder 1 "Backend" { p, q, r }; folder 2
    /// "Frontend" { z }. Deliberately wider than `folder_fixture` so a run has
    /// interior slots as well as both edges.
    fn slots_fixture() -> (crate::chrome::FrameModel, crate::compositor::Rect) {
        let model = crate::chrome::FrameModel {
            sidebar_rows: vec![
                ws_row("app"),
                wt_row("app", "home", 0),
                wt_row("app", "a", 1),
                wt_row("app", "b", 2),
                wt_row("app", "c", 3),
                folder_row("app", 1, "Backend"),
                filed_row("app", "p", 1, 4),
                filed_row("app", "q", 1, 5),
                filed_row("app", "r", 1, 6),
                folder_row("app", 2, "Frontend"),
                filed_row("app", "z", 2, 7),
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
            rows: 40,
        };
        (model, rect)
    }

    /// Every screen row the row `pin` occupies — so a rule that only works on
    /// one line of a 2-line row cannot pass.
    fn y_span(
        model: &crate::chrome::FrameModel,
        rect: crate::compositor::Rect,
        pin: &str,
    ) -> Vec<usize> {
        let hits = hit_rows(model, rect);
        let visible: Vec<&SidebarRow> = model.sidebar_rows.iter().filter(|r| r.visible).collect();
        let h = hits
            .iter()
            .find(|h| visible[h.visible_index].pin_key == pin)
            .unwrap_or_else(|| panic!("row {pin} is not on screen"));
        (h.y..h.y + h.height).collect()
    }

    /// The worktree rows of `slug`, as `(pin_key, path)` in display order.
    fn members(model: &crate::chrome::FrameModel, slug: &str) -> Vec<(String, String)> {
        model
            .sidebar_rows
            .iter()
            .filter(|r| r.visible && r.kind == RowKind::Worktree && r.workspace_slug == slug)
            .filter_map(|r| Some((r.pin_key.clone(), r.worktree_path.clone()?)))
            .collect()
    }

    /// The contract, as executable arithmetic: remove `src` from wherever it
    /// is, then insert it at the index the hovered member occupied BEFORE the
    /// drop. Flatten the runs back into the workspace's order.
    fn oracle(
        rows: &[SidebarRow],
        slug: &str,
        src_path: &str,
        dest_run: usize,
        at: usize,
    ) -> Vec<String> {
        let mut runs = crate::sidebar_order::runs(rows, slug);
        let (ri, mi) = runs
            .iter()
            .enumerate()
            .find_map(|(ri, r)| {
                r.members
                    .iter()
                    .position(|m| m.path == src_path)
                    .map(|mi| (ri, mi))
            })
            .expect("source is in the workspace");
        let m = runs[ri].members.remove(mi);
        let at = at.min(runs[dest_run].members.len());
        runs[dest_run].members.insert(at, m);
        runs.iter()
            .flat_map(|r| r.members.iter().map(|m| m.path.clone()))
            .collect()
    }

    /// Locate a worktree path as `(run index, index within that run)`.
    fn locate_in_runs(rows: &[SidebarRow], slug: &str, path: &str) -> Option<(usize, usize)> {
        crate::sidebar_order::runs(rows, slug)
            .iter()
            .enumerate()
            .find_map(|(ri, r)| {
                r.members
                    .iter()
                    .position(|m| m.path == path)
                    .map(|mi| (ri, mi))
            })
    }

    /// Drive one hover and return the workspace's resulting order, or `None`
    /// when the drop is refused / a no-op.
    fn drop_order(
        model: &crate::chrome::FrameModel,
        rect: crate::compositor::Rect,
        src: &DragSrc,
        my: usize,
    ) -> Option<Vec<String>> {
        let spot = spot_at(model, rect, src, my);
        match resolve_reorder(model, src, &spot)? {
            Resolved::Worktree(plan) => Some(plan.order),
            Resolved::Folders(_) | Resolved::Workspaces(_) => None,
        }
    }

    fn wt_src(slug: &str, pin: &str, path: &str) -> DragSrc {
        DragSrc::Worktree {
            pin_key: pin.into(),
            slug: slug.into(),
            path: path.into(),
        }
    }

    #[test]
    fn dropping_on_a_row_lands_the_source_in_that_rows_slot() {
        let mut bad: Vec<String> = Vec::new();
        for g in GEOMS {
            let (mut model, rect) = slots_fixture();
            with_geom(&mut model, *g);
            let all = members(&model, "app");
            for (src_pin, src_path) in &all {
                if src_pin.ends_with("/home") {
                    continue; // home is an anchor, not a drag source
                }
                let src = wt_src("app", src_pin, src_path);
                let (src_ri, src_mi) =
                    locate_in_runs(&model.sidebar_rows, "app", src_path).expect("source located");
                for (tgt_pin, tgt_path) in &all {
                    if tgt_pin == src_pin || tgt_pin.ends_with("/home") {
                        continue; // hovering yourself, or the anchored home row
                    }
                    let (ri, h) = locate_in_runs(&model.sidebar_rows, "app", tgt_path)
                        .expect("target located");
                    let want = oracle(&model.sidebar_rows, "app", src_path, ri, h);
                    // An identity move is legitimately refused (nothing changed).
                    let identity = ri == src_ri && h == src_mi;
                    for my in y_span(&model, rect, tgt_pin) {
                        let got = drop_order(&model, rect, &src, my);
                        if identity && got.is_none() {
                            continue;
                        }
                        if got.as_deref() != Some(want.as_slice()) {
                            bad.push(format!(
                                "  {g:?}: {src_pin} onto {tgt_pin} (row {my})\n    want {want:?}\n    got  {got:?}"
                            ));
                        }
                    }
                }
            }
        }
        assert!(
            bad.is_empty(),
            "a drop must land the source in the hovered row's slot — {} case(s) did not:\n{}",
            bad.len(),
            bad.join("\n")
        );
    }

    #[test]
    fn every_destination_slot_in_a_run_is_reachable_including_the_tail() {
        let mut bad: Vec<String> = Vec::new();
        for g in GEOMS {
            let (mut model, rect) = slots_fixture();
            with_geom(&mut model, *g);
            let all = members(&model, "app");
            for (src_pin, src_path) in &all {
                if src_pin.ends_with("/home") {
                    continue;
                }
                let src = wt_src("app", src_pin, src_path);
                let runs = crate::sidebar_order::runs(&model.sidebar_rows, "app");
                let (src_ri, src_mi) =
                    locate_in_runs(&model.sidebar_rows, "app", src_path).expect("source located");
                let run = &runs[src_ri];
                // Slot 0 of the loose run belongs to the anchored `home` row.
                let first = usize::from(run.members.first().is_some_and(|m| m.home));
                let last = run.members.len() - 1;

                // The source can always stay put, so its own index is reachable.
                let mut reached = std::collections::BTreeSet::from([src_mi]);
                for (tgt_pin, tgt_path) in &all {
                    if tgt_pin == src_pin {
                        continue;
                    }
                    // Only rows of the source's OWN run address its slots.
                    if locate_in_runs(&model.sidebar_rows, "app", tgt_path)
                        .is_none_or(|(ri, _)| ri != src_ri)
                    {
                        continue;
                    }
                    // ANY cell of the row counts — the user can aim at any of
                    // them, so a slot only counts as unreachable when no cell
                    // of any row reaches it.
                    for my in y_span(&model, rect, tgt_pin) {
                        let Some(order) = drop_order(&model, rect, &src, my) else {
                            continue;
                        };
                        // Where did the source end up within its run?
                        let run_paths: Vec<&String> = order
                            .iter()
                            .filter(|p| run.members.iter().any(|m| &m.path == *p))
                            .collect();
                        if let Some(i) = run_paths.iter().position(|p| *p == src_path) {
                            reached.insert(i);
                        }
                    }
                }
                let want: std::collections::BTreeSet<usize> = (first..=last).collect();
                if reached != want {
                    bad.push(format!(
                        "  {g:?}: {src_pin} (run slot {src_mi}) reached {reached:?}, \
                         unreachable {:?}",
                        want.difference(&reached).collect::<Vec<_>>()
                    ));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "every slot of a run must be reachable, including the tail — \
             {} source(s) could not reach every slot:\n{}",
            bad.len(),
            bad.join("\n")
        );
    }

    /// The rows must not move under a held pointer.
    ///
    /// A press hands focus to the sidebar, which under the default
    /// `FocusDetail::All` grows every branch-bearing worktree row from one line
    /// to two — so without the layout freeze a perfectly still pointer resolves
    /// to a DIFFERENT row on the first drag sample than the one it was pressed
    /// on, and every row below the cursor shifts down.
    #[test]
    fn focus_arriving_after_the_press_does_not_move_the_rows_under_the_pointer() {
        let (mut model, rect) = slots_fixture();
        model.sidebar_focused = false; // resting: all rows one line
        let mut sb = SidebarState::default();
        let session = crate::session::Session::default();

        // What the user is looking at when they press.
        let painted: Vec<(usize, String)> = hit_rows(&model, rect)
            .iter()
            .flat_map(|h| (h.y..h.y + h.height).map(|y| (y, h.pin_key.clone())))
            .collect();

        let y = y_of(&model, rect, "app/b");
        let mut ui = MouseUi::default();
        on_left_press(
            &mut ui,
            &mut sb,
            &mut model,
            &session,
            rect,
            1,
            y,
            false,
            Instant::now(),
        );
        assert!(ui.drag_active(), "the press armed a drag");

        // Replay what the loop does next iteration (run.rs, just before render).
        sb.focused = true;
        model.sidebar_focused = true;
        model.sidebar_drag_lock = ui.layout_lock();
        sb.sync(&mut model);

        let after: std::collections::HashMap<usize, String> = hit_rows(&model, rect)
            .iter()
            .flat_map(|h| (h.y..h.y + h.height).map(|y| (y, h.pin_key.clone())))
            .collect();
        for (y, pin) in &painted {
            assert_eq!(
                after.get(y),
                Some(pin),
                "screen row {y} was {pin} when pressed and must still be {pin} mid-drag"
            );
        }

        // …and the gesture therefore targets the row the user aimed at.
        let ty = y_of(&model, rect, "app/c");
        on_drag_move(&mut ui, &mut sb, &mut model, rect, ty);
        let DragPhase::Dragging { src, spot } = &ui.drag else {
            panic!("expected a live drag, got {:?}", ui.drag);
        };
        let Some(Resolved::Worktree(plan)) = resolve_reorder(&model, src, spot) else {
            panic!("expected a resolved plan");
        };
        assert_eq!(
            plan.order[..4],
            [
                "/wt/home".to_string(),
                "/wt/a".into(),
                "/wt/c".into(),
                "/wt/b".into()
            ],
            "b must take c's slot"
        );

        // The freeze lifts with the gesture.
        on_release(&mut ui, &mut model);
        assert!(ui.layout_lock().is_none());
    }

    /// Row height is a function of (rail, kind, focused, `FocusDetail`, detail
    /// content) and nothing else — the matrix every drag test is parameterised
    /// over. Locked here so a future drop test cannot be "fixed" by inflating a
    /// row the way `a_bottom_half_drop_stops_at_the_run_boundary` was.
    #[test]
    fn sidebar_row_heights_follow_the_focus_detail_policy_exactly() {
        use thegn_core::config::FocusDetail as F;
        let (mut model, rect) = slots_fixture();
        let height = |model: &crate::chrome::FrameModel, pin: &str| -> usize {
            let hits = hit_rows(model, rect);
            let visible: Vec<&SidebarRow> =
                model.sidebar_rows.iter().filter(|r| r.visible).collect();
            hits.iter()
                .find(|h| visible[h.visible_index].pin_key == pin)
                .map(|h| h.height)
                .unwrap()
        };
        // `a` carries a branch, so it has a detail line to expand into; headers
        // never do.
        for detail in [F::Off, F::Cursor, F::All] {
            model.sidebar_display.focus_detail = detail;
            model.sidebar_focused = false;
            assert_eq!(
                height(&model, "app/a"),
                1,
                "unfocused is always 1 ({detail:?})"
            );
            assert_eq!(
                height(&model, "app"),
                1,
                "a header is always 1 ({detail:?})"
            );
            assert_eq!(height(&model, "app/folder:1"), 1, "ditto folder headers");
        }
        model.sidebar_focused = true;
        model.sidebar_selected = 2; // the `a` row
        model.sidebar_display.focus_detail = F::Off;
        assert_eq!(height(&model, "app/a"), 1, "Off never expands");
        model.sidebar_display.focus_detail = F::Cursor;
        assert_eq!(height(&model, "app/a"), 2, "Cursor expands the cursor row");
        assert_eq!(height(&model, "app/b"), 1, "…and only the cursor row");
        model.sidebar_display.focus_detail = F::All;
        assert_eq!(height(&model, "app/a"), 2, "All expands every such row");
        assert_eq!(height(&model, "app/b"), 2, "…including non-cursor rows");
        assert_eq!(height(&model, "app"), 1, "headers still never expand");

        // Hit geometry must equal painted geometry, or clicks drift from pixels.
        let frame = crate::sidebar_view::build_sidebar(&model, rect, model.sidebar_scroll);
        assert_eq!(
            hit_rows(&model, rect)
                .iter()
                .map(|h| (h.visible_index, h.height))
                .collect::<Vec<_>>(),
            frame
                .rows
                .iter()
                .map(|p| (p.visible_index, p.height))
                .collect::<Vec<_>>(),
        );
        assert!(frame.rows.iter().all(|p| p.height >= 1));
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
