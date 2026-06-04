//! Command-line interface (clap derive).

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "superzej",
    version,
    about = "Terminal-native git-worktree IDE on zellij (sj is a short alias)"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Repo launcher: pick a recent repo or add a new one (default).
    Launch,
    /// Launch/attach the superzej session.
    Attach { session: Option<String> },
    /// Open a repo as a tab (prompts to pick if omitted).
    NewWorkspace {
        target: Option<String>,
        #[arg(long)]
        name: Option<String>,
    },
    /// Create a worktree + pane (agent picker).
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
    /// (internal) Picker run inside a new worktree pane.
    PickAgent {
        #[arg(long)]
        worktree: Option<String>,
        #[arg(long)]
        branch: Option<String>,
        #[arg(long)]
        agent: Option<String>,
    },
    /// Open a tool (lazygit/yazi/editor/diff) floating, scoped to the worktree.
    Tool {
        name: String,
        #[arg(long)]
        worktree: Option<String>,
    },
    /// Worktree dashboard (floating switcher, or pinnable --watch pane).
    Dashboard {
        #[arg(long)]
        watch: bool,
        #[arg(long)]
        inner: bool,
    },
    /// Close pane, optionally removing the worktree.
    ClosePane {
        #[arg(long = "remove-worktree")]
        remove_worktree: bool,
        #[arg(long = "delete-branch")]
        delete_branch: bool,
        #[arg(long)]
        force: bool,
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
    /// Worktree inventory + key hints (home tab).
    Status,
}
