//! Context-sensitive help-bar hints for the panel accordion: the per-section
//! (chord, label) pairs the statusbar chips render. Extracted from the
//! ratchet-pinned `chrome.rs` so new sections/keys can add hints without
//! growing it.

/// The context-sensitive help-bar hints for the accordion's current state, as
/// (chord, label) pairs for the statusbar's chip renderer: section-walking
/// keys while the cursor is on the section list, the open section's row
/// actions once Enter drops into its rows.
pub(crate) fn panel_help_pairs(ui: &crate::panel::PanelUi) -> Vec<(String, String)> {
    use crate::panel::Section;
    if !ui.row_mode {
        let jumps = format!("1-{}", ui.visible_section_count());
        return [
            ("j/k", "section"),
            (jumps.as_str(), "jump"),
            ("↵", "open"),
            ("⇥", "tabs"),
            ("e", "expand"),
        ]
        .iter()
        .map(|(c, l)| (c.to_string(), l.to_string()))
        .collect();
    }
    // The git-family lists draw their hints from the focused context's key
    // table (the same data that drives dispatch and the `?` cheatsheet, so
    // the help bar can never drift). The Pr section keeps its PR actions.
    if ui.open.is_git_family() && ui.open != Section::Pr {
        let ctx_keys = crate::panel::gitui::context_keys(ui.git.focus);
        let mut pairs: Vec<(String, String)> = Vec::new();
        // Sequencer flow hint leads: it replaces the generic "m flow menu" in
        // the table so the label reflects what `m` will actually do right now.
        if let Some((chord, label)) = crate::panel::gitui::flow_hint(&ui.git.flow) {
            pairs.push((chord.to_string(), label.to_string()));
            pairs.extend(
                ctx_keys
                    .iter()
                    .filter(|ck| ck.chord != chord)
                    .take(6usize.saturating_sub(1))
                    .map(|ck| (ck.chord.to_string(), ck.label.to_string())),
            );
        } else {
            pairs.extend(
                ctx_keys
                    .iter()
                    .take(6)
                    .map(|ck| (ck.chord.to_string(), ck.label.to_string())),
            );
        }
        return pairs;
    }
    // Per-section keys come from the single-source table, which is checked
    // against the real dispatch arms by `section_keys`' drift test.
    let pairs: Vec<(String, String)> = crate::panel::section_keys::section_keys(ui.open)
        .iter()
        .map(|sk| (sk.chord.to_string(), sk.label.to_string()))
        .collect();
    // "esc back" leads every row-mode hint list so the exit path is always visible.
    let mut result: Vec<(String, String)> = vec![("esc".to_string(), "back".to_string())];
    result.extend(pairs);
    result
}
