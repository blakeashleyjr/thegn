//! Command-line interface (clap derive).

use crate::github::MergeMethod;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "superzej",
    version,
    about = "Terminal-native git-worktree IDE on zellij (sj is a short alias)"
)]
pub struct Cli {
    /// Use this config file instead of $XDG_CONFIG_HOME/superzej/config.toml.
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Override the default log level for this run (error|warn|info|debug|trace).
    /// `SUPERZEJ_LOG` (tracing-style filter) takes precedence for fine control.
    #[arg(long, global = true, value_name = "LEVEL")]
    pub log_level: Option<String>,
    /// Override a config value (e.g. `--set theme.accent=cyan --set drawer.height=15`)
    #[arg(long = "set", global = true, value_name = "KEY=VALUE")]
    pub overrides: Vec<String>,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Repo launcher: pick a recent repo or add a new one (default).
    Launch,
    /// Reattach to / switch to an existing session (or launch if none given).
    Attach { session: Option<String> },
    /// Open a repo as a workspace = its own zellij session (picks if omitted).
    NewWorkspace {
        target: Option<String>,
        #[arg(long)]
        name: Option<String>,
        /// When no target is given, pick a repo via an fzf browser over $HOME
        /// (the sidebar's "+ new workspace" uses this).
        #[arg(long = "from-home")]
        from_home: bool,
    },
    /// Create a worktree as a new tab (prompts for what to run).
    NewWorktree {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long = "in-place")]
        in_place: bool,
        /// Create the worktree for a specific repo (the sidebar's "+ worktree"),
        /// given its path. Defaults to the current tab's repo.
        #[arg(long)]
        repo: Option<String>,
    },
    /// Deprecated alias for `new-worktree` (kept one release).
    #[command(hide = true)]
    NewPane {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long, default_value = "right")]
        dir: String,
        #[arg(long = "in-place")]
        in_place: bool,
    },
    /// Open a plain split pane (a "panel") in the focused worktree.
    NewPanel {
        #[arg(long, default_value = "right")]
        dir: String,
        /// Become the panel: drop to a shell at the worktree root in THIS
        /// pane (the Alt+N keybind opens the Run pane with a direction).
        #[arg(long = "in-place")]
        in_place: bool,
    },
    /// Open a second full-chrome tab on the current worktree ("{tab} ·N").
    NewTab {
        /// Session name (passed by the tabbar plugin pipe; plugin-spawned
        /// commands can't rely on env/cwd).
        #[arg(long)]
        session: Option<String>,
    },
    /// (internal) TSV of registered workspaces for the sidebar plugin —
    /// `session_name<TAB>name<TAB>repo_path` per line, including stopped ones.
    Workspaces,
    /// (internal) TSV of managed on-disk worktrees for the sidebar plugin —
    /// `repo_slug<TAB>branch_label<TAB>worktree_path` per line.
    Worktrees,
    /// (internal) Open an existing managed worktree as a tab (sidebar select).
    OpenWorktree {
        #[arg(long)]
        path: String,
    },
    /// The Cmd+K command palette: a fuzzy menu of superzej actions.
    Menu {
        /// (internal) Resolve cwd to this tab's worktree before opening. The
        /// Super+K statusbar toggle spawns the palette from a plugin, so the new
        /// pane's cwd is NOT the focused worktree; without this, worktree-scoped
        /// actions (diff/pr/lazygit/…) and the file/grep sources target the
        /// wrong tree. Omitted when run directly from a worktree shell.
        #[arg(long)]
        tab: Option<String>,
    },
    /// Pre-grant zellij plugin permissions for the sidebar + panel (setup).
    GrantPlugins,
    /// (internal) Print the worktree path for a session+tab (for the panel).
    ResolveWorktree {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        tab: Option<String>,
    },
    /// (internal) One-shot JSON snapshot (cached PR + diff) for the panel's fast
    /// first paint; also records the focused worktree for the watch daemon.
    PanelSnapshot {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        tab: Option<String>,
    },
    /// (internal) Per-session daemon: fs-watch the focused worktree and push
    /// live diff/PR updates to the panel. Auto-spawned by `attach`.
    Watch {
        #[arg(long)]
        session: Option<String>,
        /// Seconds between PR refreshes (defaults to `[watch] pr_interval_secs`).
        #[arg(long = "pr-interval")]
        pr_interval: Option<u64>,
    },
    /// (internal) Recreate the previous session's worktree tabs from the DB
    /// (each relaunches its recorded agent). Run once at cold start.
    RestoreSession,
    /// (internal) Picker run inside a new worktree tab's first pane.
    PickAgent {
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        agent: Option<String>,
        /// Accepted for layout compatibility (the worktree-tab pane passes it);
        /// pick-agent always runs in its own pane, so this is a no-op.
        #[arg(long = "in-place")]
        in_place: bool,
        /// On session restore: relaunch the worktree's previously-recorded agent
        /// instead of prompting (falls back to the picker if none is recorded).
        #[arg(long)]
        resume: bool,
    },
    /// Open a tool (lazygit/yazi/editor/diff) floating, scoped to the worktree.
    Tool {
        name: String,
        #[arg(long)]
        worktree: Option<String>,
        /// For `editor`: open this file instead of the worktree directory.
        #[arg(long)]
        file: Option<String>,
    },
    /// (internal) Open a resource monitor for a top-bar stat, embedded as a
    /// tiled pane. `kind` is `cpu`/`mem` (system monitor) or `gpu` (nvtop).
    Monitor { kind: String },
    /// Toggle the bottom file-manager drawer (yazi) for the focused worktree.
    Files {
        /// Open the drawer focused on this file (reveal), not the worktree dir.
        #[arg(long)]
        reveal: Option<String>,
        /// Target worktree path (defaults to the focused worktree).
        #[arg(long)]
        worktree: Option<String>,
        /// Resolve the worktree from this tab name via the DB (restore path).
        #[arg(long)]
        tab: Option<String>,
        /// Session name (the statusbar restore pipe passes it; plugin-spawned
        /// commands can't rely on env to target `zellij action` / the DB).
        #[arg(long)]
        session: Option<String>,
        /// Dismiss the drawer (records it so it won't auto-restore).
        #[arg(long)]
        close: bool,
        /// Open only if it was left open for this worktree (auto-restore; never
        /// toggles closed, and runs without a launcher pane to close).
        #[arg(long)]
        restore: bool,
    },
    /// Worktree dashboard (floating switcher, or pinnable --watch pane).
    Dashboard {
        #[arg(long)]
        watch: bool,
        #[arg(long)]
        inner: bool,
    },
    /// Remove the focused worktree and close its tab.
    CloseWorktree {
        #[arg(long = "delete-branch")]
        delete_branch: bool,
        #[arg(long)]
        force: bool,
    },
    /// Close the focused pane (a plain panel; never touches worktrees).
    ClosePanel,
    /// Deprecated alias: close pane, optionally removing the worktree.
    #[command(hide = true)]
    ClosePane {
        #[arg(long = "remove-worktree")]
        remove_worktree: bool,
        #[arg(long = "delete-branch")]
        delete_branch: bool,
        #[arg(long)]
        force: bool,
    },
    /// Show or toggle the left session sidebar plugin.
    Sidebar {
        #[arg(long)]
        toggle: bool,
    },
    /// Show or toggle the right diff/PR panel plugin.
    Panel {
        #[arg(long)]
        toggle: bool,
    },
    /// Pinned programs (Alt-1..9 / tabbar chips): open as either dedicated
    /// `pin:<name>` tabs or tiled panes in the active layout, per config.
    Pin {
        #[command(subcommand)]
        action: PinAction,
    },
    /// GitHub PR data + actions for a worktree (feeds the right panel).
    Pr {
        #[command(subcommand)]
        action: PrAction,
    },
    /// Emit a colorized, non-paged git diff for a worktree.
    Diff {
        #[arg(long)]
        worktree: Option<String>,
        /// Diff against this base ref (default: the worktree's resolved base).
        #[arg(long)]
        base: Option<String>,
        /// Summary (--stat) only.
        #[arg(long)]
        stat: bool,
        /// List modified files as TSV (status\tpath).
        #[arg(long)]
        files: bool,
        /// Full diff of a single file.
        #[arg(long)]
        file: Option<String>,
    },
    /// List managed worktrees.
    List {
        #[arg(long)]
        json: bool,
    },
    /// List git repos discovered under repo_roots.
    Repos,
    /// List recently opened repos (history).
    Recent { count: Option<i64> },
    /// (Internal) Dump configured hints as JSON for the statusbar.
    #[clap(hide = true)]
    Hints,
    /// Worktree inventory + key hints.
    Status,
    /// Inspect the effective (layered) configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Inspect / regenerate superzej keybindings.
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// (internal) Theme values for the plugins — one line, the accent "R;G;B".
    Theme,
    /// (internal) System stats for the tabbar widget — one line of
    /// `cpu=NN mem=NN gpu=NN time=HH:MM` (percents; gpu dropped if unreadable).
    Stats {
        /// Output stats configuration as JSON (icons and refresh rates).
        #[arg(long)]
        config: bool,
    },
    /// (internal) Terminal-activity state per worktree for the sidebar dots —
    /// `tab<TAB>state<TAB>quiet_secs` per line (state: none|active|quiet|acked).
    Activity {
        /// Acknowledge a quiet worktree (its tab was focused): clears the dot.
        #[arg(long)]
        ack: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum PinAction {
    /// Launch-or-focus a pin by name or 1-based index. Tab pins focus/open their
    /// `pin:<name>` tab; layout pins add a tiled pane to the active layout.
    Open {
        target: String,
        /// Session name (passed by the tabbar plugin; plugin-spawned commands
        /// cannot rely on env/cwd to target the right zellij session).
        #[arg(long)]
        session: Option<String>,
    },
    /// (internal) Run the focused pin tab's command in place — the `pin-tab`
    /// layout's center pane invokes this; it resolves the pin from the tab name.
    Exec {
        /// Accepted for layout compatibility (the pin-tab pane passes it).
        #[arg(long = "in-place")]
        in_place: bool,
    },
    /// Close a running pin tab (unpin from runtime).
    Close {
        target: String,
        /// Session name (passed by the tabbar plugin).
        #[arg(long)]
        session: Option<String>,
    },
    /// List configured pins as JSON (the tabbar's pin-chip feed).
    List {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
pub enum PrAction {
    /// PR + checks + review state as JSON (the panel's primary feed).
    Status {
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        json: bool,
        /// Bypass the cache and force a live gh fetch.
        #[arg(long)]
        refresh: bool,
    },
    /// Background loop: refresh status on a timer, pipe JSON to the panel.
    Watch {
        #[arg(long)]
        worktree: Option<String>,
        /// Seconds between refreshes (defaults to `[watch] pr_interval_secs`).
        #[arg(long)]
        interval: Option<u64>,
    },
    /// Create a PR from the worktree's branch.
    Create {
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        draft: bool,
        #[arg(long)]
        web: bool,
        #[arg(long)]
        fill: bool,
    },
    /// Open the PR in a browser.
    Open {
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Approve the PR.
    Approve {
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        body: Option<String>,
    },
    /// Merge the PR.
    Merge {
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long, value_enum, default_value = "squash")]
        method: MergeMethod,
        #[arg(long = "delete-branch")]
        delete_branch: bool,
        #[arg(long)]
        auto: bool,
    },
    /// Re-run failed checks for the head commit.
    RerunChecks {
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Print review comments (JSON).
    Reviews {
        #[arg(long)]
        worktree: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum ConfigAction {
    /// Print the path to the config file.
    Path,
    /// Print the effective merged config (defaults < file < env < flags).
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Alias for `show` (the effective config).
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print a single value by dotted key (bare value; for scripts/plugins).
    Get {
        key: String,
        #[arg(long)]
        json: bool,
    },
    /// Open the config file in $EDITOR (seeds from the example if missing).
    Edit,
    /// Strictly validate the config file; non-zero exit on any problem.
    Validate,
    /// Print the JSON schema for editor autocomplete and validation.
    Schema,
}

#[derive(Subcommand)]
pub enum KeysAction {
    /// Table of every action: id, key(s), label, context.
    List,
    /// Print the bare chord for one action id (non-zero if unknown).
    Get { id: String },
    /// Full effective registry (builtins + overrides + custom).
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Collision + reserved-key + unknown-id check; non-zero on any problem.
    Validate,
    /// Re-render the managed keybind region in ~/.superzej/zellij.kdl.
    Sync,
    /// (internal) Statusbar hint feed: `key<TAB>label` lines.
    Hints {
        #[arg(long, default_value = "normal")]
        mode: String,
        #[arg(long, default_value = "always")]
        context: String,
    },
}
