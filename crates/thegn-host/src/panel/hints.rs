//! Context-sensitive help-bar hints for the panel accordion: the per-section
//! (chord, label) pairs the statusbar chips render. Extracted from the
//! ratchet-pinned `chrome.rs` so new sections/keys can add hints without
//! growing it.

fn dispatches_here(msg: &crate::panel::gitui::GitMsg, width: crate::layout::PanelWidth) -> bool {
    use crate::panel::gitui::GitMsg;
    crate::panel::gitui::allowed_at_width(msg, width)
        || (width == crate::layout::PanelWidth::Normal
            && matches!(
                msg,
                GitMsg::StageToggleFile | GitMsg::Commit | GitMsg::DiscardFile
            ))
}

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
        let mut pairs = vec![("j/k", "rows")];
        if n > 1 {
            pairs.extend([("J/K", "section"), ("↵", "open"), (jumps.as_str(), "jump")]);
        }
        pairs.extend([("⇥", "tabs"), ("e", "expand")]);
        return pairs
            .into_iter()
            .map(|(c, l)| (c.to_string(), l.to_string()))
            .collect();
    }
    // The git-family lists draw their hints from the focused context's key
    // table (the same data that drives dispatch and the `?` cheatsheet, so
    // the help bar can never drift). The Pr section keeps its PR actions.
    if ui.open.is_git_family() && ui.open != Section::Pr {
        // Match the gate's width policy, including messages explicitly allowed
        // to dispatch at Normal before other git actions widen the panel.
        let narrow = ui.width == crate::layout::PanelWidth::Normal;
        let ctx_keys: Vec<crate::panel::gitui::CtxKey> =
            crate::panel::gitui::context_keys(ui.git.focus)
                .into_iter()
                .filter(|ck| dispatches_here(&ck.msg, ui.width))
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
    let pairs: Vec<(String, String)> = if ui.off_reason(ui.open).is_some() {
        Vec::new()
    } else {
        crate::panel::section_keys::section_keys(ui.open)
            .iter()
            .map(|sk| (sk.chord.to_string(), sk.label.to_string()))
            .collect()
    };
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
        PanelUi {
            open: Section::Changes,
            row_mode: true,
            width,
            ..PanelUi::default()
        }
    }

    /// Regression: `space`, `c`, and `d` execute at Normal, while other git
    /// actions DWIM-widen. Keep the duplicated hint predicate aligned with the
    /// actual git gate.
    #[test]
    fn git_hints_only_advertise_keys_that_dispatch_at_this_width() {
        let narrow = panel_help_pairs(&git_row_mode(PanelWidth::Normal));
        let chords: Vec<&str> = narrow.iter().map(|(c, _)| c.as_str()).collect();
        assert!(
            chords.contains(&"e"),
            "narrow must offer the widen key: {chords:?}"
        );
        for live in ["space", "c", "d"] {
            assert!(
                chords.contains(&live),
                "`{live}` dispatches at Normal: {chords:?}"
            );
        }
        for blocked in ["b", "m"] {
            assert!(!chords.contains(&blocked), "`{blocked}` widens: {chords:?}");
        }
        let wide = panel_help_pairs(&git_row_mode(PanelWidth::Full));
        let chords: Vec<&str> = wide.iter().map(|(c, _)| c.as_str()).collect();
        assert!(chords.contains(&"space"), "{chords:?}");
    }

    #[test]
    fn normal_hint_filter_matches_the_git_gate() {
        use crate::panel::gitui::{self, GitView};
        use termwiz::input::{KeyCode, Modifiers};

        for (section, view) in [
            (Section::Changes, GitView::Files),
            (Section::Commits, GitView::Commits),
            (Section::Branches, GitView::Branches),
            (Section::Stash, GitView::Stash),
        ] {
            let ui = PanelUi {
                open: section,
                row_mode: true,
                width: PanelWidth::Normal,
                git: gitui::GitUi {
                    focus: view,
                    ..gitui::GitUi::default()
                },
                ..PanelUi::default()
            };
            for ck in gitui::context_keys(view) {
                let (key, mods) = match ck.chord {
                    "space" => (KeyCode::Char(' '), Modifiers::NONE),
                    "enter" => (KeyCode::Enter, Modifiers::NONE),
                    chord if chord.len() == 1 => {
                        let c = chord.chars().next().unwrap();
                        let mods = if c.is_ascii_uppercase() {
                            Modifiers::SHIFT
                        } else {
                            Modifiers::NONE
                        };
                        (KeyCode::Char(c), mods)
                    }
                    _ => continue,
                };
                let actual = gitui::git_key(&key, mods, &ui);
                let reaches = !matches!(actual, Some(gitui::GitMsg::WidthBlocked(..)) | None);
                assert_eq!(
                    dispatches_here(&ck.msg, PanelWidth::Normal),
                    reaches,
                    "{} `{}` hint gate differs from git_key",
                    view.label(),
                    ck.chord
                );
            }
        }
    }

    #[test]
    fn one_section_tab_hides_jump_and_section_hop() {
        let help = PanelUi {
            tab: crate::panel::PanelTab::Help,
            open: Section::Help,
            row_mode: false,
            ..PanelUi::default()
        };
        let pairs = panel_help_pairs(&help);
        assert!(
            !pairs
                .iter()
                .any(|(chord, _)| chord == "J/K" || chord == "↵")
        );
        assert!(!pairs.iter().any(|(_, label)| label == "jump"));

        let work = PanelUi {
            tab: crate::panel::PanelTab::Work,
            open: Section::Pr,
            row_mode: false,
            ..PanelUi::default()
        };
        let pairs = panel_help_pairs(&work);
        assert!(pairs.iter().any(|(chord, _)| chord == "J/K"));
        assert!(pairs.iter().any(|(chord, _)| chord == "↵"));
        assert!(pairs.iter().any(|(_, label)| label == "jump"));
    }

    #[test]
    fn section_jump_hint_never_exceeds_the_nine_digit_keys() {
        let ui = PanelUi {
            row_mode: false,
            ..PanelUi::default()
        };
        let pairs = panel_help_pairs(&ui);
        let jump = pairs
            .iter()
            .find(|(_, l)| l == "jump")
            .map(|(c, _)| c.clone())
            .unwrap();
        let n: usize = jump.trim_start_matches("1-").parse().unwrap();
        assert!(n <= 9, "{jump}");
    }
    #[test]
    fn off_section_advertises_no_section_keys() {
        let ui = PanelUi {
            row_mode: true,
            open: Section::PrQueue,
            off: vec![(Section::PrQueue, "off — set [pr_queue] enabled = true")],
            ..PanelUi::default()
        };
        assert_eq!(panel_help_pairs(&ui), vec![("esc".into(), "back".into())]);
    }
}
