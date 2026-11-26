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
    /// Whether focus is required is controlled by [`DetailWhen`].
    ///
    /// Defaults to "all". `i` cycles this setting.
    pub enum FocusDetail: "focus detail" {
        All = "all", Cursor = "cursor", Off = "off",
    } default = All;
}

config_enum! {
    /// Where the sidebar puts a project's *external* worktrees — git worktrees
    /// of a registered repo that thegn did not create (outside its checkout
    /// layout, outside every `[project.<slug>] worktree_roots` entry, and not
    /// filed into a folder). "group" folds them under one collapsible
    /// "Other (N)" row per project; "show" lists them among the project's own
    /// rows (the pre-group behaviour); "hide" omits them (and from the
    /// project's counts). The active worktree is never hidden.
    pub enum ExternalWorktrees: "external worktrees" {
        Show = "show", Group = "group", Hide = "hide",
    } default = Group;
}

config_enum! {
    /// Whether the detail line is available only while the sidebar is focused
    /// ("focused"), or remains available while another pane owns focus
    /// ("always").
    pub enum DetailWhen: "detail visibility" {
        Focused = "focused", Always = "always",
    } default = Always;
}

/// A field that can be requested in the sidebar's ordered detail line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailField {
    /// The worktree branch name.
    Branch,
    /// Commits ahead of the default branch, and optionally behind it.
    Commits,
    /// The dirty-file count.
    Dirty,
    /// The branch-vs-default-branch line statistic.
    BranchStat,
    /// The open pull request marker.
    Pr,
    /// The last commit subject.
    Subject,
    /// The approximate last-commit age.
    Age,
    /// Commits ahead of and behind the upstream branch.
    Upstream,
    /// The current pipeline stage.
    Stage,
    /// The current short activity state.
    Activity,
    /// The selected execution environment.
    Env,
    /// The sandbox profile and backend.
    Sandbox,
    /// The devcontainer status.
    Devcontainer,
    /// Whether the worktree is hibernated.
    Hibernated,
    /// The merge-queue status.
    Mq,
    /// The unread notification count.
    Unread,
    /// The worktree disk usage.
    Disk,
    /// The name of the last commit's author.
    Author,
    /// The repo-wide stash entry count (opt-in; hidden when zero).
    Stash,
    /// The latest cached CI result (opt-in; hidden while no run is cached).
    Ci,
    /// The newest linked issue id (+N when more are linked; opt-in; hidden
    /// while none is linked). Read from the local link table, no fetch.
    Issue,
}

/// One entry in the ordered sidebar detail list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetailEntry {
    /// A field supplied by thegn itself.
    Builtin(DetailField),
    /// A field supplied by a resident plugin. The plugin need not be installed
    /// on this machine; an absent producer simply contributes no value.
    Plugin { plugin: String, field: String },
}

pub(crate) fn parse_entry(value: &str) -> Option<DetailEntry> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix("plugin:") {
        let (plugin, field) = rest.split_once('/')?;
        let valid_plugin = !plugin.is_empty()
            && plugin.len() <= 64
            && plugin
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
        let valid_field = !field.is_empty()
            && field.len() <= 32
            && field
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-".contains(c));
        return (valid_plugin && valid_field).then(|| DetailEntry::Plugin {
            plugin: plugin.to_string(),
            field: field.to_string(),
        });
    }
    DetailField::parse(value).map(DetailEntry::Builtin)
}

impl DetailField {
    /// Parse a canonical detail-field name.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "branch" => Some(Self::Branch),
            "commits" => Some(Self::Commits),
            "dirty" => Some(Self::Dirty),
            "branch_stat" => Some(Self::BranchStat),
            "pr" => Some(Self::Pr),
            "subject" => Some(Self::Subject),
            "age" => Some(Self::Age),
            "upstream" => Some(Self::Upstream),
            "stage" => Some(Self::Stage),
            "activity" => Some(Self::Activity),
            "env" => Some(Self::Env),
            "sandbox" => Some(Self::Sandbox),
            "devcontainer" => Some(Self::Devcontainer),
            "hibernated" => Some(Self::Hibernated),
            "mq" => Some(Self::Mq),
            "unread" => Some(Self::Unread),
            "disk" => Some(Self::Disk),
            "author" => Some(Self::Author),
            "stash" => Some(Self::Stash),
            "ci" => Some(Self::Ci),
            "issue" => Some(Self::Issue),
            _ => None,
        }
    }

    /// Return the canonical configuration spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Branch => "branch",
            Self::Commits => "commits",
            Self::Dirty => "dirty",
            Self::BranchStat => "branch_stat",
            Self::Pr => "pr",
            Self::Subject => "subject",
            Self::Age => "age",
            Self::Upstream => "upstream",
            Self::Stage => "stage",
            Self::Activity => "activity",
            Self::Env => "env",
            Self::Sandbox => "sandbox",
            Self::Devcontainer => "devcontainer",
            Self::Hibernated => "hibernated",
            Self::Mq => "mq",
            Self::Unread => "unread",
            Self::Disk => "disk",
            Self::Author => "author",
            Self::Stash => "stash",
            Self::Ci => "ci",
            Self::Issue => "issue",
        }
    }

    /// The valid spellings, in documentation order.
    pub const fn valid_names() -> [&'static str; 21] {
        [
            "branch",
            "commits",
            "dirty",
            "branch_stat",
            "pr",
            "subject",
            "age",
            "upstream",
            "stage",
            "activity",
            "env",
            "sandbox",
            "devcontainer",
            "hibernated",
            "mq",
            "unread",
            "disk",
            "author",
            "stash",
            "ci",
            "issue",
        ]
    }

    /// The default ordered fields: every field except the two pipeline/live
    /// ones (`stage`, `activity`), which stay opt-in. A field with nothing to
    /// show renders nothing; narrow rows drop fields by the host's fixed
    /// elision priority (optional metadata first), not by this order.
    pub const fn default_order() -> [Self; 19] {
        [
            Self::Branch,
            Self::Commits,
            Self::Dirty,
            Self::BranchStat,
            Self::Pr,
            Self::Ci,
            Self::Issue,
            Self::Author,
            Self::Stash,
            Self::Subject,
            Self::Age,
            Self::Upstream,
            Self::Env,
            Self::Sandbox,
            Self::Devcontainer,
            Self::Hibernated,
            Self::Mq,
            Self::Unread,
            Self::Disk,
        ]
    }
}

/// UI/Presentation settings (`[ui]`).
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct UiConfig {
    /// Language code (e.g. "en-US", "ja-JP"). "auto" to detect from system.
    pub language: String,
    /// Ask before destructive project worktree actions (deleting a worktree from disk via the sidebar).
    #[serde(rename = "confirm_delete_project", alias = "confirm_delete_workspace")]
    pub confirm_delete_workspace: bool,
    /// Ask before Ctrl+Q / quit closes thegn (panes in the daemon keep running). Signals, the CLI and live upgrades never ask.
    pub confirm_quit: bool,
    /// Whether to display the full word for the mode chip (e.g., "Normal" instead of "N").
    pub full_mode_chip: bool,
    /// Dismiss a detail popup when the user left-clicks outside it, like Escape.
    pub dismiss_overlay_on_click_outside: bool,
    /// Sidebar project ordering: keep the manual order, or bubble the
    /// most-urgent project to the top (see [`WorkspaceSort`]).
    #[serde(rename = "sidebar_project_sort", alias = "sidebar_workspace_sort")]
    pub sidebar_workspace_sort: WorkspaceSort,
    /// Sidebar TERMINALS section visibility (see [`TerminalsSection`]).
    pub sidebar_terminals_section: TerminalsSection,
    /// Hold the sidebar's worktree order steady while you are navigating it —
    /// for as long as the sidebar has focus, and for ~2s after an `Alt+↑/↓` /
    /// `Alt+<digit>` jump made from a pane.
    ///
    /// Only the *computed* worktree sorts can move a row on their own
    /// (`attention`, from the hydration thread's tier ranks; `live`, from the
    /// activity FSM's last-active timestamp), so this is a no-op under
    /// manual/name/recent. Off ⇒ rows re-rank the instant the underlying signal
    /// changes, including mid-keystroke — which is what made a row you were
    /// walking towards move out from under the cursor.
    pub sidebar_freeze_sort: bool,
    /// Shift+Alt+↑/↓ steps *past* workspaces and terminal hosts you have
    /// collapsed instead of stopping on them (and expanding them on arrival) —
    /// a folded group is one you are not working in. Set false to visit every
    /// group, folded or not. If every other stop is collapsed the step still
    /// lands on the immediate neighbour, so the keybind never goes dead.
    pub sidebar_nav_skips_collapsed: bool,
    /// Separate one workspace's block from the next in the sidebar, so adjacent
    /// repos read as separate groups instead of one stack of bands. The
    /// separation is an alternating background tint (`panel` / `panel_alt`)
    /// covering each workspace and everything under it; off ⇒ one uniform tint.
    ///
    /// This used to lay out a blank separator ROW above each workspace header,
    /// which cost a screen row per repo — on a tree of a dozen-plus repos, a
    /// sixth of the column. The tint says the same thing in colour and costs
    /// nothing, so unlike the gap it also applies in the rail and while the `/`
    /// filter is active.
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
    /// Use a focused pane's terminal title as a tab chip label when the stored
    /// tab title is still its default ordinal.
    pub tab_title_from_terminal: bool,
    /// Show the jujutsu-colocation marker (`ĵ`) in a worktree row's right cluster
    /// when the repo has a `.jj/` beside `.git/`.
    pub sidebar_show_jj: bool,
    /// Show a separate indicator when a submodule pointer or checkout is dirty.
    pub sidebar_show_submodules: bool,
    /// Which rows expand to a detail line while the sidebar is focused (see
    /// [`FocusDetail`]).
    pub sidebar_focus_detail: FocusDetail,
    /// Whether detail remains visible when the sidebar is unfocused.
    pub sidebar_detail_when: DetailWhen,
    /// Show the commits-ahead badge on worktree rows.
    pub sidebar_show_commits: bool,
    /// Show the commits-behind-default-branch badge on worktree rows.
    pub sidebar_show_behind_base: bool,
    /// Template for the commits-ahead badge; `{n}` is replaced by the count.
    pub sidebar_commit_badge: String,
    /// Template for the commits-behind badge; `{n}` is replaced by the count.
    pub sidebar_behind_badge: String,
    /// Ordered detail fields; unknown names are warned about and ignored.
    /// Plugin fields use `plugin:<plugin>/<field>` where the plugin id is
    /// `[A-Za-z0-9._-]{1,64}` and the field is `[a-z0-9_-]{1,32}`.
    pub sidebar_detail_fields: Vec<String>,
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
    /// Where a project's external worktrees (git worktrees thegn did not
    /// create) go: `group` under one collapsible "Other" row, `show` inline,
    /// or `hide` (see [`ExternalWorktrees`]).
    pub sidebar_external_worktrees: ExternalWorktrees,
    /// Label of the derived group row that holds external worktrees; the row
    /// reads `"{label} ({n})"`.
    pub sidebar_other_group_label: String,
    /// Whether the "Other" group starts collapsed. Expanding or collapsing it
    /// is remembered per project.
    pub sidebar_other_group_collapsed: bool,
    /// Rows a sidebar group shows before a "show more" affordance (default
    /// 7); `0` = no limit.
    pub sidebar_group_row_limit: usize,
    /// Per-group overrides of `sidebar_group_row_limit`, keyed by the folder
    /// or group label (`{ Other = 5 }`); `0` = no limit for that group.
    /// Overrides match by group label across workspaces.
    pub sidebar_group_row_limits: std::collections::BTreeMap<String, usize>,
    /// Loose worktrees sharing a batch token (`tg/b158-…` → `b158`) fold into one
    /// collapsible row per batch once at least this many share it; `0` = off.
    pub sidebar_batch_group_min: usize,
    /// Days a finished pipeline (every dispatch merged, done, failed or
    /// abandoned) stays in the sidebar's `Pipelines` section before it ages out;
    /// `0` = never. The worktree is never deleted — `wt prune` does that.
    pub sidebar_finished_pipeline_days: u32,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            language: "auto".to_string(),
            confirm_delete_workspace: true,
            confirm_quit: true,
            full_mode_chip: true,
            dismiss_overlay_on_click_outside: true,
            sidebar_workspace_sort: WorkspaceSort::default(),
            sidebar_terminals_section: TerminalsSection::default(),
            sidebar_freeze_sort: true,
            sidebar_nav_skips_collapsed: true,
            sidebar_dividers: true,
            fullscreen_keep_masthead: true,
            fullscreen_keep_statusbar: true,
            sidebar_show_status_icon: true,
            sidebar_show_diff_stat: true,
            sidebar_show_ahead_behind: true,
            sidebar_show_pr_chip: true,
            tab_title_from_terminal: true,
            sidebar_show_jj: true,
            sidebar_show_submodules: true,
            sidebar_focus_detail: FocusDetail::default(),
            sidebar_detail_when: DetailWhen::default(),
            sidebar_show_commits: true,
            sidebar_show_behind_base: false,
            sidebar_commit_badge: "{n}c".to_string(),
            sidebar_behind_badge: "{n}b".to_string(),
            sidebar_detail_fields: DetailField::default_order()
                .into_iter()
                .map(|field| field.as_str().to_string())
                .collect(),
            sidebar_detail_branch: true,
            sidebar_detail_branch_stat: true,
            sidebar_detail_pr: true,
            sidebar_icon_ahead: String::new(),
            sidebar_icon_behind: String::new(),
            sidebar_icon_status: String::new(),
            sidebar_width: None,
            sidebar_wide_ratio: None,
            sidebar_external_worktrees: ExternalWorktrees::default(),
            sidebar_other_group_label: "Other".to_string(),
            sidebar_other_group_collapsed: true,
            sidebar_group_row_limit: 7,
            sidebar_group_row_limits: std::collections::BTreeMap::new(),
            sidebar_batch_group_min: 2,
            sidebar_finished_pipeline_days: 7,
        }
    }
}

impl UiConfig {
    /// Resolve the ordered detail list, dropping unknown and duplicate names.
    /// Legacy field toggles remain an AND gate for their corresponding fields.
    pub fn detail_fields_resolved(&self) -> Vec<DetailField> {
        self.detail_entries_resolved()
            .into_iter()
            .filter_map(|entry| match entry {
                DetailEntry::Builtin(field) => Some(field),
                DetailEntry::Plugin { .. } => None,
            })
            .collect()
    }

    /// Resolve the ordered detail list, retaining valid plugin entries even
    /// when their producer is not installed. Duplicate built-in and plugin
    /// entries are dropped while preserving the first occurrence.
    pub fn detail_entries_resolved(&self) -> Vec<DetailEntry> {
        let mut resolved = Vec::new();
        for value in &self.sidebar_detail_fields {
            let Some(entry) = parse_entry(value) else {
                config_warn(&format!(
                    "ui.sidebar_detail_fields: unknown field {value:?}; expected a built-in name or plugin:<name>/<field>"
                ));
                continue;
            };
            let enabled = match entry {
                DetailEntry::Builtin(DetailField::Branch) => self.sidebar_detail_branch,
                DetailEntry::Builtin(DetailField::BranchStat) => self.sidebar_detail_branch_stat,
                DetailEntry::Builtin(DetailField::Pr) => self.sidebar_detail_pr,
                DetailEntry::Builtin(_) | DetailEntry::Plugin { .. } => true,
            };
            if enabled && !resolved.contains(&entry) {
                resolved.push(entry);
            }
        }
        resolved
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

    /// Defaults ON: a row moving out from under the cursor mid-navigation is a
    /// bug in every mode, so the freeze is the behaviour and the key is the
    /// escape hatch.
    #[test]
    fn sidebar_freeze_sort_defaults_on_and_toggles() {
        assert!(UiConfig::default().sidebar_freeze_sort);
        let cfg: UiConfig = toml::from_str("").unwrap();
        assert!(cfg.sidebar_freeze_sort);
        let cfg: UiConfig = toml::from_str("sidebar_freeze_sort = false").unwrap();
        assert!(!cfg.sidebar_freeze_sort);
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
        assert!(cfg.confirm_quit);
        assert_eq!(cfg.language, "auto");
    }

    #[test]
    fn focus_detail_defaults_to_all_and_zzz_degrades_to_all() {
        assert_eq!(
            FocusDetail::from_str_validated("all").unwrap(),
            FocusDetail::All
        );
        assert_eq!(
            FocusDetail::from_str_validated("off").unwrap(),
            FocusDetail::Off
        );
        assert!(FocusDetail::from_str_validated("bogus").is_err());
        assert_eq!(FocusDetail::default(), FocusDetail::All);
        // Unknown value degrades to the default with a warning, not an error.
        let cfg: UiConfig = toml::from_str("sidebar_focus_detail = \"zzz\"").unwrap();
        assert_eq!(cfg.sidebar_focus_detail, FocusDetail::All);
    }

    #[test]
    fn sidebar_commit_detail_keys_default_and_parse() {
        let cfg = UiConfig::default();
        assert!(cfg.sidebar_show_commits);
        assert!(!cfg.sidebar_show_behind_base);
        assert_eq!(cfg.sidebar_commit_badge, "{n}c");
        assert_eq!(cfg.sidebar_behind_badge, "{n}b");
        assert_eq!(
            cfg.detail_fields_resolved(),
            DetailField::default_order().to_vec()
        );
        let cfg: UiConfig = toml::from_str(
            "sidebar_show_commits = false\nsidebar_show_behind_base = true\nsidebar_commit_badge = \"#{n}\"\nsidebar_behind_badge = \"{n}{n}\"\nsidebar_detail_fields = [\"age\", \"branch\"]",
        )
        .unwrap();
        assert!(!cfg.sidebar_show_commits);
        assert!(cfg.sidebar_show_behind_base);
        assert_eq!(cfg.sidebar_commit_badge, "#{n}");
        assert_eq!(
            cfg.detail_fields_resolved(),
            vec![DetailField::Age, DetailField::Branch]
        );
    }

    #[test]
    fn detail_fields_resolve_order_dedup_and_drop_unknown() {
        let cfg: UiConfig =
            toml::from_str("sidebar_detail_fields = [\"age\", \"bogus\", \"branch\", \"age\"]")
                .unwrap();
        assert_eq!(
            cfg.detail_fields_resolved(),
            vec![DetailField::Age, DetailField::Branch]
        );
        let cfg: UiConfig = toml::from_str("sidebar_detail_fields = []").unwrap();
        assert!(cfg.detail_fields_resolved().is_empty());
    }

    #[test]
    fn detail_field_names_roundtrip() {
        for name in DetailField::valid_names() {
            assert_eq!(
                DetailField::parse(name).map(DetailField::as_str),
                Some(name)
            );
        }
        assert_eq!(DetailField::parse(" Branch "), Some(DetailField::Branch));
        assert_eq!(DetailField::parse("stash"), Some(DetailField::Stash));
        assert_eq!(DetailField::Stash.as_str(), "stash");
        assert!(DetailField::default_order().contains(&DetailField::Stash));
        assert_eq!(DetailField::parse("ci"), Some(DetailField::Ci));
        assert_eq!(DetailField::Ci.as_str(), "ci");
        assert!(DetailField::valid_names().contains(&"ci"));
        assert!(DetailField::default_order().contains(&DetailField::Ci));
        assert_eq!(DetailField::parse("issue"), Some(DetailField::Issue));
        assert_eq!(DetailField::parse(" Issue "), Some(DetailField::Issue));
        assert_eq!(DetailField::Issue.as_str(), "issue");
        assert_eq!(DetailField::valid_names().last(), Some(&"issue"));
        assert!(DetailField::default_order().contains(&DetailField::Issue));
    }

    #[test]
    fn detail_entries_parse_plugin_syntax_and_preserve_order() {
        let cfg: UiConfig = toml::from_str(
            r#"sidebar_detail_fields = ["age", "plugin:ci/check", "branch", "plugin:ci/check", "plugin:missing/badge"]"#,
        )
        .unwrap();
        assert_eq!(
            cfg.detail_entries_resolved(),
            vec![
                DetailEntry::Builtin(DetailField::Age),
                DetailEntry::Plugin {
                    plugin: "ci".into(),
                    field: "check".into()
                },
                DetailEntry::Builtin(DetailField::Branch),
                DetailEntry::Plugin {
                    plugin: "missing".into(),
                    field: "badge".into()
                },
            ]
        );
    }

    #[test]
    fn detail_entries_drop_malformed_plugin_values() {
        let long_id = "a".repeat(200);
        let cfg: UiConfig = toml::from_str(&format!(
            "sidebar_detail_fields = [\"plugin:\", \"plugin:a\", \"plugin:a/\", \"plugin:/field\", \"plugin:A/UPPER\", \"plugin:{long_id}/field\", \"plugin:ok/field\"]"
        ))
        .unwrap();
        assert_eq!(
            cfg.detail_entries_resolved(),
            vec![DetailEntry::Plugin {
                plugin: "ok".into(),
                field: "field".into()
            }]
        );
    }

    #[test]
    fn detail_when_parses_and_unknown_degrades() {
        assert_eq!(DetailWhen::default(), DetailWhen::Always);
        let cfg: UiConfig = toml::from_str("sidebar_detail_when = \"focused\"").unwrap();
        assert_eq!(cfg.sidebar_detail_when, DetailWhen::Focused);
        let cfg: UiConfig = toml::from_str("sidebar_detail_when = \"zzz\"").unwrap();
        assert_eq!(cfg.sidebar_detail_when, DetailWhen::Always);
    }

    #[test]
    fn sidebar_row_display_defaults_on_and_toggle() {
        let cfg = UiConfig::default();
        assert!(cfg.tab_title_from_terminal);
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
        let cfg: UiConfig = toml::from_str("tab_title_from_terminal = false").unwrap();
        assert!(!cfg.tab_title_from_terminal);
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

    /// The owner-chosen default (b139): every field but `stage` and
    /// `activity`, in this exact order. Every entry must be a valid spelling.
    #[test]
    fn detail_default_order_is_the_nineteen_field_list() {
        let names: Vec<&str> = DetailField::default_order()
            .into_iter()
            .map(DetailField::as_str)
            .collect();
        assert_eq!(
            names,
            [
                "branch",
                "commits",
                "dirty",
                "branch_stat",
                "pr",
                "ci",
                "issue",
                "author",
                "stash",
                "subject",
                "age",
                "upstream",
                "env",
                "sandbox",
                "devcontainer",
                "hibernated",
                "mq",
                "unread",
                "disk",
            ]
        );
        for name in &names {
            assert!(DetailField::valid_names().contains(name), "{name}");
        }
        let opt_in: Vec<&str> = DetailField::valid_names()
            .into_iter()
            .filter(|n| !names.contains(n))
            .collect();
        assert_eq!(opt_in, ["stage", "activity"]);
        assert_eq!(
            UiConfig::default().sidebar_detail_fields,
            names.iter().map(|n| n.to_string()).collect::<Vec<_>>()
        );
        // An explicit empty list still means "no fields".
        let cfg: UiConfig = toml::from_str("sidebar_detail_fields = []").unwrap();
        assert!(cfg.detail_fields_resolved().is_empty());
    }

    #[test]
    fn external_worktree_keys_default_and_parse() {
        let cfg = UiConfig::default();
        assert_eq!(cfg.sidebar_external_worktrees, ExternalWorktrees::Group);
        assert_eq!(cfg.sidebar_other_group_label, "Other");
        assert!(cfg.sidebar_other_group_collapsed);
        assert_eq!(cfg.sidebar_group_row_limit, 7);
        assert!(cfg.sidebar_group_row_limits.is_empty());
        // Defaults survive an empty table.
        let cfg: UiConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.sidebar_external_worktrees, ExternalWorktrees::Group);
        assert_eq!(cfg.sidebar_group_row_limit, 7);
        for (raw, want) in [
            ("show", ExternalWorktrees::Show),
            ("group", ExternalWorktrees::Group),
            ("hide", ExternalWorktrees::Hide),
        ] {
            assert_eq!(ExternalWorktrees::from_str_validated(raw).unwrap(), want);
        }
        assert!(ExternalWorktrees::from_str_validated("bogus").is_err());
        assert_eq!(cfg.sidebar_batch_group_min, 2);
        assert_eq!(UiConfig::default().sidebar_batch_group_min, 2);
        let cfg: UiConfig = toml::from_str("sidebar_batch_group_min = 0").unwrap();
        assert_eq!(cfg.sidebar_batch_group_min, 0);
        let cfg: UiConfig = toml::from_str("sidebar_batch_group_min = 4").unwrap();
        assert_eq!(cfg.sidebar_batch_group_min, 4);
        let pretty = r#"
sidebar_external_worktrees = "hide"
sidebar_other_group_label = "Elsewhere"
sidebar_other_group_collapsed = false
sidebar_group_row_limit = 0

[sidebar_group_row_limits]
Other = 5
"agents wip" = 0
"#;
        let minified = r#"sidebar_external_worktrees="hide"
sidebar_other_group_label="Elsewhere"
sidebar_other_group_collapsed=false
sidebar_group_row_limit=0
sidebar_group_row_limits={Other=5,"agents wip"=0}"#;
        for src in [pretty, minified] {
            let cfg: UiConfig = toml::from_str(src).unwrap();
            assert_eq!(cfg.sidebar_external_worktrees, ExternalWorktrees::Hide);
            assert_eq!(cfg.sidebar_other_group_label, "Elsewhere");
            assert!(!cfg.sidebar_other_group_collapsed);
            assert_eq!(cfg.sidebar_group_row_limit, 0);
            assert_eq!(cfg.sidebar_group_row_limits.get("Other"), Some(&5));
            assert_eq!(cfg.sidebar_group_row_limits.get("agents wip"), Some(&0));
        }
        // Unknown enum value degrades to the default with a warning.
        let cfg: UiConfig = toml::from_str("sidebar_external_worktrees = \"zzz\"").unwrap();
        assert_eq!(cfg.sidebar_external_worktrees, ExternalWorktrees::Group);
    }

    /// An `[ui]` table serialized before `sidebar_batch_group_min` existed keeps
    /// the limit it wrote and takes the new key's default.
    #[test]
    fn an_old_config_without_the_batch_key_keeps_the_new_defaults() {
        let cfg: UiConfig = toml::from_str("sidebar_group_row_limit = 15").unwrap();
        assert_eq!(cfg.sidebar_group_row_limit, 15);
        assert_eq!(cfg.sidebar_batch_group_min, 2);
        assert_eq!(cfg.sidebar_finished_pipeline_days, 7);
    }
}
