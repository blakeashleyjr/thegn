//! Row-activation dispatch for the panel's changes section — extracted from the
//! event loop (`run.rs`) to keep that god-file under its size ratchet.

use crate::chrome::FrameModel;

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
