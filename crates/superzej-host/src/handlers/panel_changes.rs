//! Row-activation dispatch for the panel's changes section — extracted from the
//! event loop (`run.rs`) to keep that god-file under its size ratchet.

use crate::chrome::FrameModel;
use crate::compositor::Rect;
use crate::focus::{FocusState, Zone};
use crate::panes::Panes;
use crate::run::SidebarState;
use crate::session::Session;

/// Loop state the semantic drill-in needs to open an entity in a new center
/// tab. Mirrors the `CiActionCtx` idiom so `run.rs` (a size-ratcheted god-file)
/// stays lean — the editor-open body lives here, not in the event loop.
pub(crate) struct EntityOpenCtx<'a> {
    pub session: &'a mut Session,
    pub panes: &'a mut Panes,
    pub model: &'a mut FrameModel,
    pub focus: &'a mut FocusState,
    pub sb: &'a mut SidebarState,
    pub need_relayout: &'a mut bool,
    pub center: Rect,
    pub cfg: &'a superzej_core::config::Config,
}

/// Drill into the entity at hit index `i` of the expanded semantic breakdown:
/// resolve its `(path, line)` via [`superzej_core::semantic::EntitySummary::entity_targets`]
/// (which mirrors the renderer's row order one-for-one) and open the file in the
/// editor at that line, exactly like a failing-test jump. Indices at or below
/// the footer (`changes.len()`) are file rows / the footer itself, not entity
/// rows, so they're ignored. Returns whether it opened an editor.
pub(crate) fn open_entity_at(i: usize, ctx: EntityOpenCtx<'_>) -> bool {
    // File rows own `0..changes.len()`; the footer owns `changes.len()`; entity
    // rows start one past it, aligned with `entity_targets()`.
    let base = ctx.model.panel.changes.len() + 1;
    if i < base {
        return false;
    }
    let Some((path, line)) = ctx
        .model
        .panel
        .entities
        .as_ref()
        .and_then(|e| e.entity_targets().into_iter().nth(i - base))
    else {
        return false;
    };
    let cmd = crate::panel_util::editor_open_command(ctx.cfg, &path, Some(line as usize));
    let cwd = crate::run::active_cwd(ctx.session);
    crate::actions::open_command_tab(ctx.session, ctx.panes, &cmd, cwd.as_deref(), ctx.center);
    ctx.focus.zone = Zone::Center;
    crate::run::refresh_tab_model(ctx.model, ctx.session, ctx.sb);
    *ctx.need_relayout = true;
    true
}

/// Everything the Changes-section Enter/activation path touches, bundled so the
/// event loop (`run.rs`, a size-ratcheted god-file) calls one function instead
/// of inlining the branch logic. Mirrors the `CiActionCtx` idiom.
pub(crate) struct ChangesActivateCtx<'a> {
    pub panel_ui: &'a mut crate::panel::PanelUi,
    pub model: &'a mut FrameModel,
    pub session: &'a mut Session,
    pub panes: &'a mut Panes,
    pub focus: &'a mut FocusState,
    pub sb: &'a mut SidebarState,
    pub need_relayout: &'a mut bool,
    pub center: Rect,
    pub cfg: &'a superzej_core::config::Config,
    pub hunk_inflight: &'a mut std::collections::HashSet<String>,
    pub hunk_tx:
        &'a tokio::sync::mpsc::UnboundedSender<(u64, String, Vec<superzej_svc::git::Hunk>)>,
    pub waker: &'a termwiz::terminal::TerminalWaker,
    pub hydration_gen: u64,
}

/// Activate the cursor row of the changes section on Enter. Dispatches by row:
/// a **conflict** file opens in the editor for resolution; an **entity** row in
/// the expanded semantic breakdown (`cursor > changes.len()`) drills into its
/// definition; the **semantic footer** (`cursor == changes.len()`) toggles the
/// breakdown and widens a resting panel so it's readable; any other **file** row
/// toggles its inline diff preview.
pub(crate) fn select_changes_row(ctx: ChangesActivateCtx<'_>) {
    let cursor = ctx.panel_ui.cursor;
    let n = ctx.model.panel.changes.len();

    let is_conflict = ctx
        .model
        .panel
        .changes
        .get(cursor)
        .is_some_and(|c| c.stage == crate::panel::Stage::Conflict);
    if is_conflict {
        if let Some(path) = ctx.model.panel.changes.get(cursor).map(|c| c.path.clone()) {
            let cmd = crate::panel_util::editor_open_command(ctx.cfg, &path, None);
            let cwd = crate::run::active_cwd(ctx.session);
            crate::actions::open_command_tab(
                ctx.session,
                ctx.panes,
                &cmd,
                cwd.as_deref(),
                ctx.center,
            );
            ctx.focus.zone = Zone::Center;
            crate::run::refresh_tab_model(ctx.model, ctx.session, ctx.sb);
            *ctx.need_relayout = true;
        }
        return;
    }

    if cursor > n {
        open_entity_at(
            cursor,
            EntityOpenCtx {
                session: ctx.session,
                panes: ctx.panes,
                model: ctx.model,
                focus: ctx.focus,
                sb: ctx.sb,
                need_relayout: ctx.need_relayout,
                center: ctx.center,
                cfg: ctx.cfg,
            },
        );
        return;
    }

    let on_footer = cursor == n;
    toggle_change_selection(
        cursor,
        ctx.panel_ui,
        ctx.model,
        ctx.session,
        ctx.hunk_inflight,
        ctx.hunk_tx,
        ctx.waker,
        ctx.hydration_gen,
    );
    // Expanding the semantic footer widens a resting panel so the per-entity
    // breakdown is readable (like Files / Notifications).
    if on_footer
        && ctx.panel_ui.impact_open
        && ctx.panel_ui.width == crate::layout::PanelWidth::Normal
    {
        ctx.panel_ui.width = crate::layout::PanelWidth::Half;
        *ctx.need_relayout = true;
    }
}

/// Activate row `i` of the changes section.
///
/// The change-file rows own hit indices `0..changes.len()`; the *semantic-impact
/// footer* is the one row past them (hit index `changes.len()`), so activating it
/// toggles its inline breakdown (`impact_open`) rather than a file preview. For a
/// file row this toggles the selection onto row `i` (re-selecting dismisses the
/// preview) and kicks the background hunk fetch for newly-selected paths.
#[allow(clippy::too_many_arguments)]
pub(crate) fn toggle_change_selection(
    i: usize,
    panel_ui: &mut crate::panel::PanelUi,
    model: &FrameModel,
    session: &crate::session::Session,
    hunk_inflight: &mut std::collections::HashSet<String>,
    hunk_tx: &tokio::sync::mpsc::UnboundedSender<(u64, String, Vec<superzej_svc::git::Hunk>)>,
    waker: &termwiz::terminal::TerminalWaker,
    generation: u64,
) {
    // The semantic-impact footer sits one past the last file row.
    if i == model.panel.changes.len() && model.panel.entities.is_some() {
        panel_ui.impact_open = !panel_ui.impact_open;
        return;
    }
    if panel_ui.chg_sel == Some(i) {
        panel_ui.chg_sel = None;
        return;
    }
    panel_ui.chg_sel = Some(i);
    // Untracked rows have no diff: the preview renders a static note.
    if let Some(row) = model
        .panel
        .changes
        .get(i)
        .filter(|c| c.stage != crate::panel::Stage::Untracked)
    {
        crate::run::spawn_hunk_fetch(
            &row.path,
            session,
            panel_ui,
            hunk_inflight,
            hunk_tx,
            waker,
            generation,
        );
    }
}
