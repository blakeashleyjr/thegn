//! Row-activation dispatch for the panel's changes section — extracted from the
//! event loop (`run.rs`) to keep that god-file flat.

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
    pub cfg: &'a thegn_core::config::Config,
}

/// Drill into the entity at hit index `i` of the expanded semantic breakdown:
/// resolve its `(path, line)` via [`thegn_core::semantic::EntitySummary::entity_targets`]
/// (which mirrors the renderer's row order one-for-one) and open the file in the
/// editor at that line, exactly like a failing-test jump. `visible_files` is
/// [`crate::panel::visible_change_files`] for the current view — file rows own
/// `0..visible_files`, the footer owns `visible_files`, entity rows start one
/// past it. Indices at or below the footer are ignored. Returns whether it
/// opened an editor.
pub(crate) fn open_entity_at(i: usize, visible_files: usize, ctx: EntityOpenCtx<'_>) -> bool {
    let base = visible_files + 1;
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
    let cwd = crate::run::active_cwd(ctx.session);
    if crate::panel_util::open_editor(
        ctx.session,
        ctx.panes,
        ctx.cfg,
        &path,
        Some(line as usize),
        cwd.as_deref(),
        ctx.center,
        None,
    ) {
        ctx.focus.zone = Zone::Center;
    }
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
    pub cfg: &'a thegn_core::config::Config,
    pub hunk_inflight: &'a mut PreviewOwners,
    pub hunk_tx: &'a tokio::sync::mpsc::UnboundedSender<crate::run::HunkMsg>,
    pub submodule_inflight: &'a mut PreviewOwners,
    pub submodule_tx: &'a tokio::sync::mpsc::UnboundedSender<SubmodulePreviewMsg>,
    pub waker: &'a termwiz::terminal::TerminalWaker,
    pub hydration_gen: u64,
}

/// Activate the cursor row of the changes section on Enter. Dispatches by row:
/// a **conflict** file opens in the editor for resolution; an **entity** row in
/// the expanded semantic breakdown (`cursor > changes.len()`) drills into its
/// definition; the **semantic footer** (`cursor == changes.len()`) toggles the
/// breakdown and widens a resting panel so it's readable; any other **file** row
/// toggles its summary preview. A plain file row is reserved for the git
/// staging drill dispatched by the row-mode key handler.
pub(crate) fn select_changes_row(ctx: ChangesActivateCtx<'_>) {
    let cursor = ctx.panel_ui.cursor;
    // Boundaries in the VISIBLE cursor space (collapsed merge-incoming rows
    // are not file rows here) — see `visible_change_files`.
    let deep = ctx.panel_ui.width != crate::layout::PanelWidth::Normal;
    let n = crate::panel::visible_change_files(&ctx.model.panel, deep);

    let is_conflict = cursor < n
        && ctx
            .model
            .panel
            .changes
            .get(cursor)
            .is_some_and(|c| c.stage == crate::panel::Stage::Conflict && c.submodule.is_none());
    if is_conflict {
        if let Some(path) = ctx.model.panel.changes.get(cursor).map(|c| c.path.clone()) {
            let cwd = crate::run::active_cwd(ctx.session);
            if crate::panel_util::open_editor(
                ctx.session,
                ctx.panes,
                ctx.cfg,
                &path,
                None,
                cwd.as_deref(),
                ctx.center,
                None,
            ) {
                ctx.focus.zone = Zone::Center;
            }
            crate::run::refresh_tab_model(ctx.model, ctx.session, ctx.sb);
            *ctx.need_relayout = true;
        }
        return;
    }

    if cursor > n {
        open_entity_at(
            cursor,
            n,
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
    if cursor < n
        && ctx
            .model
            .panel
            .changes
            .get(cursor)
            .is_some_and(|row| row.submodule.is_none())
    {
        return;
    }
    toggle_change_selection(
        cursor,
        n,
        ctx.panel_ui,
        ctx.model,
        ctx.session,
        ctx.hunk_inflight,
        ctx.hunk_tx,
        ctx.submodule_inflight,
        ctx.submodule_tx,
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
        ctx.panel_ui.remember_width();
        *ctx.need_relayout = true;
    }
}

/// Keep the inline preview on the keyboard cursor at Half width. At the
/// resting width the staging-first interaction has no inline preview.
#[allow(clippy::too_many_arguments)]
pub(crate) fn follow_change_cursor(
    panel_ui: &mut crate::panel::PanelUi,
    model: &FrameModel,
    session: &Session,
    hunk_inflight: &mut PreviewOwners,
    hunk_tx: &tokio::sync::mpsc::UnboundedSender<crate::run::HunkMsg>,
    submodule_inflight: &mut PreviewOwners,
    submodule_tx: &tokio::sync::mpsc::UnboundedSender<SubmodulePreviewMsg>,
    waker: &termwiz::terminal::TerminalWaker,
    generation: u64,
) {
    match follow_change_cursor_target(panel_ui, model) {
        Some(None) => panel_ui.chg_sel = None,
        Some(Some(cursor)) => toggle_change_selection(
            cursor,
            crate::panel::visible_change_files(&model.panel, true),
            panel_ui,
            model,
            session,
            hunk_inflight,
            hunk_tx,
            submodule_inflight,
            submodule_tx,
            waker,
            generation,
        ),
        None => {}
    }
}

fn follow_change_cursor_target(
    panel_ui: &crate::panel::PanelUi,
    model: &FrameModel,
) -> Option<Option<usize>> {
    if panel_ui.open != crate::panel::Section::Changes {
        return None;
    }
    if panel_ui.width == crate::layout::PanelWidth::Normal {
        return Some(None);
    }
    if panel_ui.width != crate::layout::PanelWidth::Half || !panel_ui.row_mode {
        return None;
    }
    let visible = crate::panel::visible_change_files(&model.panel, true);
    (panel_ui.cursor < visible && panel_ui.chg_sel != Some(panel_ui.cursor))
        .then_some(Some(panel_ui.cursor))
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
    visible_files: usize,
    panel_ui: &mut crate::panel::PanelUi,
    model: &FrameModel,
    session: &crate::session::Session,
    hunk_inflight: &mut PreviewOwners,
    hunk_tx: &tokio::sync::mpsc::UnboundedSender<crate::run::HunkMsg>,
    submodule_inflight: &mut PreviewOwners,
    submodule_tx: &tokio::sync::mpsc::UnboundedSender<SubmodulePreviewMsg>,
    waker: &termwiz::terminal::TerminalWaker,
    generation: u64,
) {
    // The semantic-impact footer sits one past the last VISIBLE file row.
    if i == visible_files && model.panel.entities.is_some() {
        panel_ui.impact_open = !panel_ui.impact_open;
        return;
    }
    // Never select past the visible file rows (a collapsed incoming file).
    if i > visible_files {
        return;
    }
    if panel_ui.chg_sel == Some(i) {
        panel_ui.chg_sel = None;
        return;
    }
    panel_ui.chg_sel = Some(i);
    start_selected_preview(
        panel_ui,
        model,
        session,
        hunk_inflight,
        hunk_tx,
        submodule_inflight,
        submodule_tx,
        waker,
        generation,
    );
}

#[derive(Debug, PartialEq, Eq)]
enum PreviewTarget {
    Hunk(String),
    Submodule(usize),
}

fn selected_preview_target(
    panel_ui: &crate::panel::PanelUi,
    model: &FrameModel,
) -> Option<PreviewTarget> {
    let index = panel_ui.chg_sel?;
    let row = model.panel.changes.get(index)?;
    if let Some(submodule) = row.submodule.as_ref() {
        return submodule
            .summary
            .is_none()
            .then_some(PreviewTarget::Submodule(index));
    }
    if row.stage == crate::panel::Stage::Untracked || panel_ui.hunks_banked(&row.path) {
        return None;
    }
    Some(PreviewTarget::Hunk(row.path.clone()))
}

/// Start a preview for the current selection if it is eligible and has a free
/// per-kind slot. Completions call this again so only the latest selection is
/// retried after capacity becomes available.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start_selected_preview(
    panel_ui: &crate::panel::PanelUi,
    model: &FrameModel,
    session: &Session,
    hunk_inflight: &mut PreviewOwners,
    hunk_tx: &tokio::sync::mpsc::UnboundedSender<crate::run::HunkMsg>,
    submodule_inflight: &mut PreviewOwners,
    submodule_tx: &tokio::sync::mpsc::UnboundedSender<SubmodulePreviewMsg>,
    waker: &termwiz::terminal::TerminalWaker,
    generation: u64,
) {
    match selected_preview_target(panel_ui, model) {
        Some(PreviewTarget::Hunk(path)) => crate::run::spawn_hunk_fetch(
            &path,
            session,
            panel_ui,
            hunk_inflight,
            hunk_tx,
            waker,
            generation,
        ),
        Some(PreviewTarget::Submodule(index)) => {
            if let Some(row) = model.panel.changes.get(index) {
                spawn_submodule_preview(
                    row,
                    session,
                    submodule_inflight,
                    submodule_tx,
                    waker,
                    generation,
                );
            }
        }
        None => {}
    }
}

/// Monotonic ownership tokens prevent late workers from clearing a replacement
/// job for the same path after a refresh or worktree switch.
#[derive(Debug, Default)]
pub(crate) struct PreviewOwners {
    next: u64,
    owners: std::collections::HashMap<String, u64>,
    running: usize,
}

/// Maximum blocking Git preview jobs allowed per kind at once.
pub(crate) const MAX_ACTIVE_PREVIEW_JOBS: usize = 4;

impl PreviewOwners {
    pub(crate) fn begin(&mut self, key: &str) -> Option<u64> {
        if self.owners.contains_key(key) {
            return None;
        }
        if self.running >= MAX_ACTIVE_PREVIEW_JOBS {
            return None;
        }
        self.next = self.next.checked_add(1)?;
        self.owners.insert(key.to_owned(), self.next);
        self.running = self.running.saturating_add(1);
        Some(self.next)
    }

    pub(crate) fn finish(&mut self, key: &str, token: u64) -> bool {
        self.running = self.running.saturating_sub(1);
        if self.owners.get(key) != Some(&token) {
            return false;
        }
        self.owners.remove(key);
        true
    }

    pub(crate) fn clear(&mut self) {
        self.owners.clear();
    }
}

pub(crate) type SubmodulePreviewMsg = (u64, u64, String, thegn_core::submodule::SubmoduleSummary);

/// Fetch a bounded, local-object-only gitlink summary off the event loop.
/// Failures become an explicit unavailable summary so the preview keeps the
/// two pointer IDs and never spins or fetches indefinitely.
fn spawn_submodule_preview(
    row: &crate::panel::ChangeRow,
    session: &crate::session::Session,
    inflight: &mut PreviewOwners,
    tx: &tokio::sync::mpsc::UnboundedSender<SubmodulePreviewMsg>,
    waker: &termwiz::terminal::TerminalWaker,
    generation: u64,
) {
    const LIMIT: usize = 8;
    let Some(submodule) = row
        .submodule
        .as_ref()
        .filter(|submodule| submodule.summary.is_none())
    else {
        return;
    };
    let Some(token) = inflight.begin(&row.path) else {
        return;
    };
    let wt = crate::hydrate::active_tab_path(session);
    let path = row.path.clone();
    let old = submodule.diff.old_sha.clone();
    let new = submodule.diff.new_sha.clone();
    let tx = tx.clone();
    let waker = waker.clone();
    tokio::task::spawn_blocking(move || {
        use thegn_svc::git::GitBackend;
        let loc = submodule_loc(thegn_core::remote::GitLoc::for_worktree(&wt), &path);
        let summary = thegn_svc::git::CliGit
            .submodule_summary(&loc, &path, &old, &new, LIMIT)
            .unwrap_or_else(|_| {
                thegn_core::submodule::SubmoduleSummary::bounded(
                    thegn_core::submodule::SubmoduleDirection::Unknown,
                    Vec::new(),
                    LIMIT,
                    true,
                )
            });
        if tx.send((generation, token, path, summary)).is_ok() && waker.wake().is_err() {
            tracing::debug!("submodule preview wake dropped during shutdown");
        }
    });
}

/// Point the summary read at the nested repository. The backend keeps the
/// path parameter for implementations that can use it directly; the CLI
/// backend runs its bounded log in the supplied location.
fn submodule_loc(
    loc: thegn_core::remote::GitLoc,
    submodule_path: &str,
) -> thegn_core::remote::GitLoc {
    use thegn_core::remote::GitLoc;
    match loc {
        GitLoc::Local(root) => GitLoc::Local(root.join(submodule_path)),
        GitLoc::Remote { ssh, path } => GitLoc::Remote {
            ssh,
            path: format!("{}/{}", path.trim_end_matches('/'), submodule_path),
        },
        GitLoc::Provider {
            control_prefix,
            path,
        } => GitLoc::Provider {
            control_prefix,
            path: format!("{}/{}", path.trim_end_matches('/'), submodule_path),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FrameModel, MAX_ACTIVE_PREVIEW_JOBS, PreviewOwners, PreviewTarget,
        follow_change_cursor_target, selected_preview_target,
    };

    #[test]
    fn begin_refuses_past_the_active_job_cap() {
        let mut owners = PreviewOwners::default();
        let tokens: Vec<_> = (0..MAX_ACTIVE_PREVIEW_JOBS)
            .map(|index| owners.begin(&format!("path-{index}")).unwrap())
            .collect();

        assert_eq!(owners.begin("fifth-path"), None);
        assert!(owners.finish("path-0", tokens[0]));
        assert!(owners.begin("fifth-path").is_some());
    }

    #[test]
    fn cleared_jobs_hold_their_slot_until_they_report() {
        let mut owners = PreviewOwners::default();
        let tokens: Vec<_> = (0..MAX_ACTIVE_PREVIEW_JOBS)
            .map(|index| owners.begin(&format!("path-{index}")).unwrap())
            .collect();

        owners.clear();
        assert_eq!(owners.begin("new-path"), None);
        assert!(!owners.finish("path-0", tokens[0]));
        assert!(owners.begin("new-path").is_some());
    }

    #[test]
    fn selected_preview_target_picks_the_selected_unbanked_row() {
        let mut model = FrameModel::default();
        let mut ui = crate::panel::PanelUi::default();
        assert_eq!(selected_preview_target(&ui, &model), None);
        ui.chg_sel = Some(0);
        assert_eq!(selected_preview_target(&ui, &model), None);

        model.panel.changes.push(crate::panel::ChangeRow {
            path: "a.rs".into(),
            ..Default::default()
        });
        ui.chg_sel = Some(0);
        assert_eq!(
            selected_preview_target(&ui, &model),
            Some(PreviewTarget::Hunk("a.rs".into()))
        );
        ui.bank_hunks("a.rs".into(), Ok(Vec::new()));
        assert_eq!(selected_preview_target(&ui, &model), None);

        model.panel.changes[0].stage = crate::panel::Stage::Untracked;
        ui.clear_hunks();
        assert_eq!(selected_preview_target(&ui, &model), None);

        model.panel.changes[0] = crate::panel::ChangeRow {
            path: "submodule".into(),
            submodule: Some(crate::panel::SubmoduleChange {
                diff: thegn_core::submodule::SubmoduleDiff {
                    path: "submodule".into(),
                    old_sha: "old".into(),
                    new_sha: "new".into(),
                    kind: thegn_core::submodule::SubmoduleDiffKind::Changed,
                },
                state: None,
                summary: None,
            }),
            ..Default::default()
        };
        assert_eq!(
            selected_preview_target(&ui, &model),
            Some(PreviewTarget::Submodule(0))
        );
        model.panel.changes[0]
            .submodule
            .as_mut()
            .expect("submodule fixture")
            .summary = Some(thegn_core::submodule::SubmoduleSummary::bounded(
            thegn_core::submodule::SubmoduleDirection::Unknown,
            Vec::new(),
            8,
            true,
        ));
        assert_eq!(selected_preview_target(&ui, &model), None);
    }

    #[test]
    fn duplicate_path_owner_is_refused_until_completed() {
        let mut owners = PreviewOwners::default();
        let first = owners.begin("src/lib.rs").unwrap();
        assert_eq!(owners.begin("src/lib.rs"), None);
        assert!(owners.finish("src/lib.rs", first));
    }

    #[test]
    fn stale_completion_cannot_remove_a_replacement_owner() {
        let mut owners = PreviewOwners::default();
        let stale = owners.begin("src/lib.rs").unwrap();
        owners.clear();
        let current = owners.begin("src/lib.rs").unwrap();

        assert!(!owners.finish("src/lib.rs", stale));
        assert_eq!(owners.begin("src/lib.rs"), None);
        assert!(owners.finish("src/lib.rs", current));
    }

    #[test]
    fn follow_change_cursor_selects_at_half_only() {
        let mut model = FrameModel::default();
        model.panel.changes.push(crate::panel::ChangeRow {
            path: "f.rs".into(),
            ..Default::default()
        });
        let mut ui = crate::panel::PanelUi {
            open: crate::panel::Section::Changes,
            width: crate::layout::PanelWidth::Half,
            row_mode: true,
            ..Default::default()
        };
        assert_eq!(follow_change_cursor_target(&ui, &model), Some(Some(0)));
        ui.width = crate::layout::PanelWidth::Full;
        assert_eq!(follow_change_cursor_target(&ui, &model), None);
        ui.width = crate::layout::PanelWidth::Normal;
        assert_eq!(follow_change_cursor_target(&ui, &model), Some(None));
    }
}
