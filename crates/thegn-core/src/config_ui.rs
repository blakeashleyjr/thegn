//! UI/presentation `[ui]` config.
//!
//! Lives in this sibling module (rather than the pinned `config.rs` god-file)
//! and is re-exported from `config`, so the canonical
//! `thegn_core::config::UiConfig` path keeps working — the same pattern as
//! `config_theme`.

use crate::config::{config_enum, config_warn};
use serde::{Deserialize, Serialize};

config_enum! {
    /// How workspaces (repos) order in the sidebar. "manual" preserves the
    /// user's persisted order (`workspaces.position`, Ctrl+Alt+↑/↓);
    /// "attention" bubbles the workspace whose worktrees are most urgent to
    /// the top (stable within a tier, so equal-urgency workspaces keep their
    /// manual order and rows only move on a real state change). Worktree
    /// ordering *within* a workspace is the separate, session-scoped sort
    /// mode (`s` in the sidebar).
    pub enum WorkspaceSort: "workspace sort" {
        Manual = "manual", Attention = "attention",
    } default = Manual;
}

config_enum! {
    /// When the sidebar shows its TERMINALS section. "always" keeps the banner
    /// (and its "no terminals" hint) visible so the entry point never silently
    /// vanishes; "nonempty" hides the whole section until a terminal exists.
    pub enum TerminalsSection: "terminals section" {
        Always = "always", NonEmpty = "nonempty",
    } default = Always;
}

/// UI/Presentation settings (`[ui]`).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct UiConfig {
    /// Language code (e.g. "en-US", "ja-JP"). "auto" to detect from system.
    pub language: String,
    /// Ask before destructive worktree actions (deleting a worktree from disk via the sidebar).
    pub confirm_delete_workspace: bool,
    /// Whether to display the full word for the mode chip (e.g., "Normal" instead of "N").
    pub full_mode_chip: bool,
    /// Dismiss a detail popup when the user left-clicks outside it, like Escape.
    pub dismiss_overlay_on_click_outside: bool,
    /// Sidebar workspace ordering: keep the manual order, or bubble the
    /// most-urgent workspace to the top (see [`WorkspaceSort`]).
    pub sidebar_workspace_sort: WorkspaceSort,
    /// Sidebar TERMINALS section visibility (see [`TerminalsSection`]).
    pub sidebar_terminals_section: TerminalsSection,
    /// In full-window pane fullscreen (the third stop of Ctrl+Alt+z, which
    /// hides the sidebar/panel/strip), keep the top masthead bar visible.
    pub fullscreen_keep_masthead: bool,
    /// In full-window pane fullscreen, keep the bottom status bar visible.
    pub fullscreen_keep_statusbar: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            language: "auto".to_string(),
            confirm_delete_workspace: true,
            full_mode_chip: true,
            dismiss_overlay_on_click_outside: true,
            sidebar_workspace_sort: WorkspaceSort::default(),
            sidebar_terminals_section: TerminalsSection::default(),
            fullscreen_keep_masthead: true,
            fullscreen_keep_statusbar: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_sort_parses_and_defaults_manual() {
        assert_eq!(
            WorkspaceSort::from_str_validated("attention").unwrap(),
            WorkspaceSort::Attention
        );
        assert_eq!(
            WorkspaceSort::from_str_validated("manual").unwrap(),
            WorkspaceSort::Manual
        );
        assert!(WorkspaceSort::from_str_validated("bogus").is_err());
        assert_eq!(WorkspaceSort::default(), WorkspaceSort::Manual);
        assert_eq!(
            UiConfig::default().sidebar_workspace_sort,
            WorkspaceSort::Manual
        );
    }

    #[test]
    fn terminals_section_parses_and_defaults_always() {
        assert_eq!(
            TerminalsSection::from_str_validated("nonempty").unwrap(),
            TerminalsSection::NonEmpty
        );
        assert!(TerminalsSection::from_str_validated("bogus").is_err());
        assert_eq!(TerminalsSection::default(), TerminalsSection::Always);
        let cfg: UiConfig = toml::from_str("sidebar_terminals_section = \"nonempty\"").unwrap();
        assert_eq!(cfg.sidebar_terminals_section, TerminalsSection::NonEmpty);
        assert_eq!(
            UiConfig::default().sidebar_terminals_section,
            TerminalsSection::Always
        );
    }

    #[test]
    fn ui_config_toml_roundtrip_with_new_key() {
        let cfg: UiConfig = toml::from_str("sidebar_workspace_sort = \"attention\"").unwrap();
        assert_eq!(cfg.sidebar_workspace_sort, WorkspaceSort::Attention);
        // Unknown enum value degrades to the default with a warning, not an error.
        let cfg: UiConfig = toml::from_str("sidebar_workspace_sort = \"zzz\"").unwrap();
        assert_eq!(cfg.sidebar_workspace_sort, WorkspaceSort::Manual);
        // Defaults survive an empty table.
        let cfg: UiConfig = toml::from_str("").unwrap();
        assert!(cfg.confirm_delete_workspace);
        assert_eq!(cfg.language, "auto");
    }

    #[test]
    fn fullscreen_bar_keys_default_on_and_parse() {
        // Both bars are kept by default (matches the "except top and bottom
        // bars" contract) and survive an empty table.
        let cfg = UiConfig::default();
        assert!(cfg.fullscreen_keep_masthead);
        assert!(cfg.fullscreen_keep_statusbar);
        let cfg: UiConfig = toml::from_str("").unwrap();
        assert!(cfg.fullscreen_keep_masthead);
        assert!(cfg.fullscreen_keep_statusbar);
        // Either bar can be turned off independently.
        let cfg: UiConfig =
            toml::from_str("fullscreen_keep_masthead = false\nfullscreen_keep_statusbar = false")
                .unwrap();
        assert!(!cfg.fullscreen_keep_masthead);
        assert!(!cfg.fullscreen_keep_statusbar);
        let cfg: UiConfig = toml::from_str("fullscreen_keep_masthead = false").unwrap();
        assert!(!cfg.fullscreen_keep_masthead);
        assert!(cfg.fullscreen_keep_statusbar);
    }
}
