//! User configuration, loaded from `$XDG_CONFIG_HOME/superzej/config.toml`.
//! Missing fields fall back to sensible defaults, so superzej works with no
//! config at all. The home-manager module renders this file.

use crate::util;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct NamedCommand {
    pub name: String,
    pub command: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub worktrees_dir: String,
    pub workspaces_dir: String,
    pub base_branch: String,
    pub branch_prefix: String,
    pub picker: String,
    pub worktree_mode: String, // "global" | "in_repo"
    pub name_scheme: String,   // "words" | "numbered"
    pub auto_remove_worktree: bool,
    pub repo_roots: Vec<String>,
    pub repo_scan_depth: usize,
    pub agents: Vec<NamedCommand>,
    pub tools: Vec<NamedCommand>,
}

impl Default for Config {
    fn default() -> Self {
        let home = util::home();
        Config {
            worktrees_dir: home.join("worktrees").to_string_lossy().into_owned(),
            workspaces_dir: home.join("code").to_string_lossy().into_owned(),
            base_branch: "auto".into(),
            branch_prefix: "sz/".into(),
            picker: "auto".into(),
            worktree_mode: "global".into(),
            name_scheme: "words".into(),
            auto_remove_worktree: false,
            repo_roots: Vec::new(),
            repo_scan_depth: 5,
            agents: Vec::new(),
            tools: Vec::new(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        util::xdg_config_home().join("superzej/config.toml")
    }

    /// Load config, applying defaults and post-processing (fallback agents/tools,
    /// default repo_roots, tilde expansion).
    pub fn load() -> Self {
        let mut cfg: Config = match std::fs::read_to_string(Self::path()) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                crate::msg::warn(&format!("config parse error: {e}; using defaults"));
                Config::default()
            }),
            Err(_) => Config::default(),
        };

        if cfg.agents.is_empty() {
            cfg.agents = vec![
                NamedCommand {
                    name: "claude".into(),
                    command: "claude".into(),
                },
                NamedCommand {
                    name: "shell".into(),
                    command: "__shell__".into(),
                },
            ];
        }
        if cfg.tools.is_empty() {
            cfg.tools = vec![
                NamedCommand {
                    name: "lazygit".into(),
                    command: "lazygit".into(),
                },
                NamedCommand {
                    name: "yazi".into(),
                    command: "yazi".into(),
                },
                NamedCommand {
                    name: "editor".into(),
                    command: "${EDITOR:-vi} .".into(),
                },
                NamedCommand {
                    name: "diff".into(),
                    command: "git diff".into(),
                },
            ];
        }

        cfg.worktrees_dir = util::expand_tilde(&cfg.worktrees_dir);
        cfg.workspaces_dir = util::expand_tilde(&cfg.workspaces_dir);
        if cfg.repo_roots.is_empty() {
            cfg.repo_roots = vec![cfg.workspaces_dir.clone()];
        }
        cfg.repo_roots = cfg
            .repo_roots
            .iter()
            .map(|r| util::expand_tilde(r))
            .collect();
        cfg
    }

    pub fn agent_command(&self, name: &str) -> Option<&str> {
        self.agents
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.command.as_str())
    }

    pub fn tool_command(&self, name: &str) -> Option<&str> {
        self.tools
            .iter()
            .find(|t| t.name == name)
            .map(|t| t.command.as_str())
    }
}
