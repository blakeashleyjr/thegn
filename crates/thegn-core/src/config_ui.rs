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

config_enum! {
    /// Which worktree rows show the secondary detail line (branch + extra info)
    /// while the sidebar is focused. "all" expands every worktree row; "cursor"
    /// expands only the highlighted row; "off" never shows the detail line. The
    /// detail line only appears while the sidebar owns focus.
    ///
    /// Defaults to "cursor". "all" doubles the height of EVERY worktree row the
    /// moment the sidebar takes focus, which on a full tree pushed the bottom
    /// rows out of the viewport — merely focusing the sidebar appeared to
    /// delete the last workspace. Scrolling now reaches them either way, but
    /// halving the visible tree on focus is a poor default. `i` cycles it.
    pub enum FocusDetail: "focus detail" {
        All = "all", Cursor = "cursor", Off = "off",
    } default = Cursor;
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
    /// Shift+Alt+↑/↓ steps *past* workspaces and terminal hosts you have
    /// collapsed instead of stopping on them (and expanding them on arrival) —
    /// a folded group is one you are not working in. Set false to visit every
    /// group, folded or not. If every other stop is collapsed the step still
    /// lands on the immediate neighbour, so the keybind never goes dead.
    pub sidebar_nav_skips_collapsed: bool,
    /// Lay out a one-row separator gap above each workspace header in the full
    /// sidebar, so adjacent repos read as separate groups instead of one stack
    /// of bands. Off ⇒ the tree lays out exactly as it did before the key
    /// existed (for vertically-tight setups: many repos, a short terminal).
    /// Never applies in the rail or while the `/` filter is active.
    pub sidebar_dividers: bool,
    /// In full-window pane fullscreen (the third stop of Ctrl+Alt+z, which
    /// hides the sidebar/panel/strip), keep the top masthead bar visible.
    pub fullscreen_keep_masthead: bool,
    /// In full-window pane fullscreen, keep the bottom status bar visible.
    pub fullscreen_keep_statusbar: bool,
    /// Show the dirty status icon in a worktree row's right cluster.
    pub sidebar_show_status_icon: bool,
    /// Show the uncommitted working-tree line stat (`+adds` green / `-dels` red)
    /// in a worktree row's right cluster.
    pub sidebar_show_diff_stat: bool,
    /// Show the `↑ahead` / `↓behind` upstream counts in a worktree row.
    pub sidebar_show_ahead_behind: bool,
    /// Show the compact open-PR chip (`⬡N`) in a worktree row's right cluster.
    pub sidebar_show_pr_chip: bool,
    /// Show the jujutsu-colocation marker (`ĵ`) in a worktree row's right cluster
    /// when the repo has a `.jj/` beside `.git/`.
    pub sidebar_show_jj: bool,
    /// Show a separate indicator when a submodule pointer or checkout is dirty.
    pub sidebar_show_submodules: bool,
    /// Which rows expand to a detail line while the sidebar is focused (see
    /// [`FocusDetail`]).
    pub sidebar_focus_detail: FocusDetail,
    /// Lead the focused detail line with the branch name.
    pub sidebar_detail_branch: bool,
    /// Show the total branch-vs-default-branch line stat on the focused detail
    /// line.
    pub sidebar_detail_branch_stat: bool,
    /// Show the open PR (`#N`) on the focused detail line.
    pub sidebar_detail_pr: bool,
    /// Override glyph for the `ahead` marker; empty = the built-in (`↑`, ASCII
    /// `^`). A non-empty override is used verbatim (no ASCII degradation).
    pub sidebar_icon_ahead: String,
    /// Override glyph for the `behind` marker; empty = the built-in (`↓`/`v`).
    pub sidebar_icon_behind: String,
    /// Override glyph for the dirty status marker; empty = the built-in (`●`/`*`).
    pub sidebar_icon_status: String,
    /// Resting sidebar width in columns. Unset = the built-in default (32).
    /// Clamped to 12–200 at apply time; a width you nudge (`<`/`>`) or drag
    /// wins over this key, and the layout still shrinks it when the screen
    /// can't afford it.
    pub sidebar_width: Option<usize>,
    /// Fraction of the window the sidebar's Wide expand (`e`) claims. Unset =
    /// 0.5. Clamped to 0.2–0.9 at apply time; never below the resting width.
    pub sidebar_wide_ratio: Option<f32>,
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
            sidebar_nav_skips_collapsed: true,
            sidebar_dividers: true,
            fullscreen_keep_masthead: true,
            fullscreen_keep_statusbar: true,
            sidebar_show_status_icon: true,
            sidebar_show_diff_stat: true,
            sidebar_show_ahead_behind: true,
            sidebar_show_pr_chip: true,
            sidebar_show_jj: true,
            sidebar_show_submodules: true,
            sidebar_focus_detail: FocusDetail::default(),
            sidebar_detail_branch: true,
            sidebar_detail_branch_stat: true,
            sidebar_detail_pr: true,
            sidebar_icon_ahead: String::new(),
            sidebar_icon_behind: String::new(),
            sidebar_icon_status: String::new(),
            sidebar_width: None,
            sidebar_wide_ratio: None,
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
    fn nav_skips_collapsed_defaults_on_and_toggles() {
        assert!(UiConfig::default().sidebar_nav_skips_collapsed);
        // Survives an empty table.
        let cfg: UiConfig = toml::from_str("").unwrap();
        assert!(cfg.sidebar_nav_skips_collapsed);
        // Opt back into the old stop-on-every-group behaviour.
        let cfg: UiConfig = toml::from_str("sidebar_nav_skips_collapsed = false").unwrap();
        assert!(!cfg.sidebar_nav_skips_collapsed);
    }

    #[test]
    fn sidebar_dividers_defaults_on_and_toggles() {
        assert!(UiConfig::default().sidebar_dividers);
        // Survives an empty table.
        let cfg: UiConfig = toml::from_str("").unwrap();
        assert!(cfg.sidebar_dividers);
        // Opt back into the old dense layout.
        let cfg: UiConfig = toml::from_str("sidebar_dividers = false").unwrap();
        assert!(!cfg.sidebar_dividers);
    }

    #[test]
    fn sidebar_width_keys_default_to_unset_and_parse() {
        // Unset means "use the built-in" — the host resolves that, not serde,
        // so a fresh config must not pin a width here.
        let cfg: UiConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.sidebar_width, None);
        assert_eq!(cfg.sidebar_wide_ratio, None);
        let cfg: UiConfig = toml::from_str("sidebar_width = 40\nsidebar_wide_ratio = 0.7").unwrap();
        assert_eq!(cfg.sidebar_width, Some(40));
        assert_eq!(cfg.sidebar_wide_ratio, Some(0.7));
        // Out-of-range values parse; clamping is the host's job at apply time
        // (mirrors `[panel] width`), so a silly number must not fail the load.
        let cfg: UiConfig = toml::from_str("sidebar_width = 9999").unwrap();
        assert_eq!(cfg.sidebar_width, Some(9999));
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
    fn focus_detail_parses_and_defaults_to_cursor() {
        assert_eq!(
            FocusDetail::from_str_validated("all").unwrap(),
            FocusDetail::All
        );
        assert_eq!(
            FocusDetail::from_str_validated("off").unwrap(),
            FocusDetail::Off
        );
        assert!(FocusDetail::from_str_validated("bogus").is_err());
        // "cursor", not "all": expanding EVERY worktree row on focus doubles
        // the list's height and pushes the bottom of the tree off screen.
        assert_eq!(FocusDetail::default(), FocusDetail::Cursor);
        // Unknown value degrades to the default with a warning, not an error.
        let cfg: UiConfig = toml::from_str("sidebar_focus_detail = \"zzz\"").unwrap();
        assert_eq!(cfg.sidebar_focus_detail, FocusDetail::Cursor);
    }

    #[test]
    fn sidebar_row_display_defaults_on_and_toggle() {
        let cfg = UiConfig::default();
        assert!(cfg.sidebar_show_status_icon);
        assert!(cfg.sidebar_show_diff_stat);
        assert!(cfg.sidebar_show_ahead_behind);
        assert!(cfg.sidebar_show_pr_chip);
        assert!(cfg.sidebar_show_jj);
        assert!(cfg.sidebar_show_submodules);
        assert!(cfg.sidebar_detail_branch);
        assert!(cfg.sidebar_icon_ahead.is_empty());
        // Defaults survive an empty table.
        let cfg: UiConfig = toml::from_str("").unwrap();
        assert!(cfg.sidebar_show_diff_stat);
        // Each toggle and icon override is independent.
        let cfg: UiConfig =
            toml::from_str("sidebar_show_diff_stat = false\nsidebar_icon_ahead = \"»\"").unwrap();
        assert!(!cfg.sidebar_show_diff_stat);
        assert!(cfg.sidebar_show_ahead_behind);
        assert_eq!(cfg.sidebar_icon_ahead, "»");
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
