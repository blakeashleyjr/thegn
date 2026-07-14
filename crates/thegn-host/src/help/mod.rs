//! help — the host half of the built-in documentation system.
//!
//! `thegn_core::help` owns the pure model (frontmatter, markdown subset,
//! registry, search); this module owns everything substrate-facing: the
//! embedded page set ([`pages`]), AST→`Line` rendering ([`render`]), the F1
//! overlay ([`overlay`]), focus→page resolution ([`context`]), and the
//! generated keybindings page ([`gen_pages`]). The ratchet test in
//! [`ratchet_tests`] keeps every action documented.

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

/// Open the overlay at the page for whatever has focus right now.
pub fn open(
    reg: &Arc<HelpRegistry>,
    focus: &crate::focus::FocusState,
    panel_ui: &crate::panel::PanelUi,
) -> Option<HelpOverlay> {
    open_at(reg, &context::resolve(focus, panel_ui))
}

/// Open the overlay at the page claiming `context_key` (falls back to
/// `index`). `None` only when the registry is empty.
pub fn open_at(reg: &Arc<HelpRegistry>, context_key: &str) -> Option<HelpOverlay> {
    let page = reg.page_for_context(context_key)?.to_string();
    Some(HelpOverlay::new(reg.clone(), page))
}
