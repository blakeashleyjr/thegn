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
        // Item-first model: plain j/k walk ROWS (and enter row mode on the
        // first press); Shift-J/K hop section headers. The old "j/k section"
        // hint predated that model and advertised the wrong key.
        let n = ui.visible_section_count();
        let jumps = if n <= 9 {
            format!("1-{n}")
        } else {
            "1-9,0".to_string()
        };
        return [
            ("j/k", "rows"),
            ("J/K", "section"),
            ("↵", "open"),
            (jumps.as_str(), "jump"),
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
        // Only keys that dispatch at the current panel width: `git_key` drops
        // every non-navigation message at Normal (and Full-only ones at Half),
        // so advertising them here was a lie the user could not act on.
        let narrow = ui.width == crate::layout::PanelWidth::Normal;
        let ctx_keys: Vec<crate::panel::gitui::CtxKey> =
            crate::panel::gitui::context_keys(ui.git.focus)
                .into_iter()
                .filter(|ck| crate::panel::gitui::allowed_at_width(&ck.msg, ui.width))
                .collect();
        let mut pairs: Vec<(String, String)> = Vec::new();
        if narrow {
            // The way to unlock the action keys.
            pairs.push(("e".to_string(), "widen".to_string()));
        }
        // Sequencer flow hint leads: it replaces the generic "m flow menu" in
        // the table so the label reflects what `m` will actually do right now.
        if let Some((chord, label)) =
            crate::panel::gitui::flow_hint(&ui.git.flow).filter(|_| !narrow)
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::PanelWidth;
    use crate::panel::{PanelUi, Section};

    fn git_row_mode(width: PanelWidth) -> PanelUi {
        let mut ui = PanelUi::default();
        ui.open = Section::Changes;
        ui.row_mode = true;
        ui.width = width;
        ui
    }

    /// Regression: at the resting (Normal) width `git_key` drops every
    /// non-navigation message, yet the bar advertised `space stage · c commit
    /// · d discard`; pressing them did nothing. Hints must follow the gate.
    #[test]
    fn git_hints_only_advertise_keys_that_dispatch_at_this_width() {
        let narrow = panel_help_pairs(&git_row_mode(PanelWidth::Normal));
        let chords: Vec<&str> = narrow.iter().map(|(c, _)| c.as_str()).collect();
        assert!(
            chords.contains(&"e"),
            "narrow must offer the widen key: {chords:?}"
        );
        for dead in ["space", "c", "d", "b", "m"] {
            assert!(
                !chords.contains(&dead),
                "`{dead}` does not dispatch at Normal: {chords:?}"
            );
        }
        let wide = panel_help_pairs(&git_row_mode(PanelWidth::Full));
        let chords: Vec<&str> = wide.iter().map(|(c, _)| c.as_str()).collect();
        assert!(chords.contains(&"space"), "{chords:?}");
    }

    #[test]
    fn section_jump_hint_never_exceeds_the_nine_digit_keys() {
        let mut ui = PanelUi::default();
        ui.row_mode = false;
        let pairs = panel_help_pairs(&ui);
        let jump = pairs
            .iter()
            .find(|(_, l)| l == "jump")
            .map(|(c, _)| c.clone())
            .unwrap();
        let n: usize = jump.trim_start_matches("1-").parse().unwrap();
        assert!(n <= 9, "{jump}");
    }
}
