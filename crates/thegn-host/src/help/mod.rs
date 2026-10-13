//! help — the host half of the built-in documentation system.
//!
//! `thegn_core::help` owns the pure model (frontmatter, markdown subset,
//! registry, search); this module owns everything substrate-facing: the
//! embedded page set ([`pages`]), AST→`Line` rendering ([`render`]), the F1
//! overlay ([`overlay`]), focus→page resolution ([`context`]), and the
//! generated keybindings page ([`gen_pages`]). The ratchet test in
//! `ratchet_tests` keeps every action documented.

pub mod context;
pub mod gen_pages;
pub mod overlay;
pub mod pages;
pub mod render;

#[cfg(test)]
mod ratchet_tests;

use std::sync::Arc;

pub use overlay::{HelpOutcome, HelpOverlay};
use thegn_core::help::HelpRegistry;

/// A width-independent position in a help page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelpAnchor {
    pub heading: usize,
    pub offset: usize,
}

/// Record the last heading at or above `scroll` and the line offset below it.
pub fn anchor_at(headings: &[(usize, String)], scroll: usize) -> Option<HelpAnchor> {
    let heading = headings.iter().rposition(|(line, _)| *line <= scroll)?;
    Some(HelpAnchor {
        heading,
        offset: scroll.saturating_sub(headings[heading].0),
    })
}

/// Resolve an anchor at the current page width, bounded by the next heading
/// and the final rendered line.
pub fn scroll_for(headings: &[(usize, String)], anchor: HelpAnchor, total_lines: usize) -> usize {
    let Some((line, _)) = headings.get(anchor.heading) else {
        return 0;
    };
    let end = headings
        .get(anchor.heading + 1)
        .map_or(total_lines.saturating_sub(1), |(next, _)| {
            next.saturating_sub(1)
        });
    line.saturating_add(anchor.offset)
        .min(end)
        .min(total_lines.saturating_sub(1))
}

/// Find a heading containing `key` or `label` as a case-insensitive phrase.
/// Markdown's section suffix is ignored, so names stay stable across prose.
pub fn heading_for_section(headings: &[(usize, String)], key: &str, label: &str) -> Option<usize> {
    let matches = |heading: &str, needle: &str| {
        let needle: Vec<String> = needle
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect();
        if needle.is_empty() {
            return false;
        }
        let heading: Vec<String> = heading
            .split(|c: char| !c.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect();
        heading.windows(needle.len()).any(|window| window == needle)
    };
    headings.iter().position(|(_, heading)| {
        let title = heading
            .split_once(" — ")
            .map_or(heading.as_str(), |(a, _)| a);
        matches(title, key) || matches(title, label)
    })
}

/// Open the overlay at the page for whatever has focus right now.
pub fn open(
    reg: &Arc<HelpRegistry>,
    cfg: &thegn_core::config::Config,
    focus: &crate::focus::FocusState,
    panel_ui: &crate::panel::PanelUi,
) -> Option<HelpOverlay> {
    let badge = crate::keymap::chord_hint_for(cfg, "help").unwrap_or_else(|| "help".to_string());
    if focus.panel() && panel_ui.open == crate::panel::Section::Help {
        let page = if panel_ui.help.page.is_empty() {
            "index"
        } else {
            panel_ui.help.page.as_str()
        };
        return Some(HelpOverlay::new_at(
            reg.clone(),
            page.to_string(),
            crate::panel::sections::help::anchor_at(panel_ui).map(overlay::OverlayTarget::Anchor),
            badge,
        ));
    }
    let context_key = context::resolve(focus, panel_ui);
    let page = reg.page_for_context(&context_key)?.to_string();
    let section_key = context_key.strip_prefix("panel:").unwrap_or("");
    let label = crate::panel::Section::from_key(section_key)
        .map(crate::panel::Section::label)
        .unwrap_or("");
    let anchor = reg.anchor_for_context(&context_key);
    Some(HelpOverlay::new_at(
        reg.clone(),
        page,
        Some(overlay::OverlayTarget::Heading {
            key: anchor.unwrap_or(section_key).to_string(),
            label: anchor.unwrap_or(label).to_string(),
        }),
        badge,
    ))
}

/// Open the overlay at the page claiming `context_key` (falls back to
/// `index`). `None` only when the registry is empty.
///
/// `cfg` resolves the badge chord: the overlay advertises the key that opens
/// it, which must follow a rebind of `help` rather than always reading `F1`.
pub fn open_at(
    reg: &Arc<HelpRegistry>,
    cfg: &thegn_core::config::Config,
    context_key: &str,
) -> Option<HelpOverlay> {
    let page = reg.page_for_context(context_key)?.to_string();
    let badge = crate::keymap::chord_hint_for(cfg, "help").unwrap_or_else(|| "help".to_string());
    Some(HelpOverlay::new(reg.clone(), page, badge))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchors_round_trip_and_clamp_between_headings() {
        let headings = vec![(2, "first".to_string()), (8, "second".to_string())];
        let anchor = anchor_at(&headings, 6).unwrap();
        assert_eq!(
            anchor,
            HelpAnchor {
                heading: 0,
                offset: 4
            }
        );
        assert_eq!(scroll_for(&headings, anchor, 20), 6);
        assert_eq!(
            scroll_for(
                &headings,
                HelpAnchor {
                    heading: 0,
                    offset: 30,
                },
                20,
            ),
            7
        );
        assert_eq!(anchor_at(&headings, 1), None);
    }

    #[test]
    fn section_heading_match_is_whole_word_and_ignores_suffix() {
        let headings = vec![
            (3, "jobs and tests — run status".to_string()),
            (8, "sandbox, hosts, environments".to_string()),
            (12, "testimonials".to_string()),
        ];
        assert_eq!(heading_for_section(&headings, "tests", "Tests"), Some(0));
        assert_eq!(heading_for_section(&headings, "hosts", "Hosts"), Some(1));
        assert_eq!(heading_for_section(&headings, "test", "Test"), None);
    }

    #[test]
    fn f1_from_docked_help_opens_the_docked_page_at_its_position() {
        let (reg, errors) = pages::build_registry(&thegn_core::config::Config::default());
        assert!(errors.is_empty(), "{errors:?}");
        let reg = Arc::new(reg);
        let model = crate::chrome::FrameModel::default();
        let ui = crate::panel::PanelUi {
            open: crate::panel::Section::Help,
            help: crate::panel::HelpPanelState {
                page: "git-and-diffs".to_string(),
                reg: Some(Arc::clone(&reg)),
            },
            scroll: 8,
            ..Default::default()
        };
        let ctx = crate::panel::sections::SectionCtx {
            model: &model,
            ui: &ui,
            cols: 42,
            rows: 16,
        };
        crate::panel::sections::help::content(&ctx);
        let panel_anchor = crate::panel::sections::help::anchor_at(&ui).unwrap();
        let focus = crate::focus::FocusState {
            zone: crate::focus::Zone::Panel,
            ..Default::default()
        };
        let mut overlay = open(&reg, &thegn_core::config::Config::default(), &focus, &ui).unwrap();
        let mut surface = termwiz::surface::Surface::new(100, 30);
        overlay.render(
            &mut surface,
            crate::compositor::Rect {
                x: 0,
                y: 0,
                cols: 100,
                rows: 30,
            },
        );
        assert_eq!(overlay.page_id(), "git-and-diffs");
        assert_eq!(overlay.anchor(), Some(panel_anchor));
    }

    #[test]
    fn every_panel_context_has_a_page_and_heading() {
        let (reg, errors) = pages::build_registry(&thegn_core::config::Config::default());
        assert!(errors.is_empty(), "{errors:?}");
        for context in context::vocabulary()
            .iter()
            .filter(|context| context.starts_with("panel:"))
        {
            let page_id = reg
                .page_for_context(context)
                .unwrap_or_else(|| panic!("{context}"));
            let page = reg.page(page_id).unwrap();
            let rendered = render::render_page(&page.blocks, 80, None);
            let heading = reg.anchor_for_context(context).or_else(|| {
                let key = context.strip_prefix("panel:").unwrap_or("");
                let label = crate::panel::Section::from_key(key)
                    .map(crate::panel::Section::label)
                    .unwrap_or("");
                let ordinal = heading_for_section(&rendered.headings, key, label)?;
                rendered
                    .headings
                    .get(ordinal)
                    .map(|(_, text)| text.as_str())
            });
            if page_id == "panel" {
                let key = context.strip_prefix("panel:").unwrap_or("");
                let label = crate::panel::Section::from_key(key)
                    .map(crate::panel::Section::label)
                    .unwrap_or("");
                let anchor = reg.anchor_for_context(context);
                let ordinal = heading_for_section(
                    &rendered.headings,
                    anchor.unwrap_or(key),
                    anchor.unwrap_or(label),
                )
                .unwrap_or_else(|| panic!("{context} on {page_id} has no section heading"));
                assert!(ordinal > 0, "{context} resolves to the page H1");
            } else if heading.is_none() {
                const ACCEPTED_H1_FALLBACKS: &[&str] = &[
                    "panel:changes",
                    "panel:commits",
                    "panel:branches",
                    "panel:stash",
                    "panel:files",
                    "panel:sandbox",
                    "panel:prq",
                    "panel:keys",
                ];
                assert!(
                    ACCEPTED_H1_FALLBACKS.contains(&context.as_str()),
                    "{context} on {page_id} has no heading and is not an accepted H1 fallback"
                );
                assert!(
                    !rendered.headings.is_empty(),
                    "{page_id} has no H1 for the accepted fallback {context}"
                );
            }
            if context == "panel:ci" {
                assert!(heading.unwrap().to_lowercase().contains("ci"));
            }
        }
    }
}
