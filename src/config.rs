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

/// `[theme]` — visual tuning. Only the accent for now; the rest of the
/// palette is fixed (src/theme.rs).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Focus accent as "#rrggbb" (default the signature teal).
    pub accent: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        ThemeConfig {
            accent: "#76eede".into(),
        }
    }
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
    pub theme: ThemeConfig,
}

impl Default for Config {
    fn default() -> Self {
        let home = util::home();
        Config {
            // Under superzej's root (honors SUPERZEJ_DIR) so a dev/test instance
            // gets its own worktrees, isolated from the daily-driver instance.
            worktrees_dir: util::superzej_dir()
                .join("worktrees")
                .to_string_lossy()
                .into_owned(),
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
            theme: ThemeConfig::default(),
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

    /// The accent as a truecolor "R;G;B" fragment; invalid hex falls back to
    /// the default teal.
    pub fn accent_rgb(&self) -> String {
        parse_hex_rgb(&self.theme.accent).unwrap_or_else(|| crate::theme::TEAL.to_string())
    }

    /// The accent as "#rrggbb" (validated; falls back to the default teal).
    pub fn accent_hex(&self) -> String {
        match parse_hex_rgb(&self.theme.accent) {
            Some(_) => self.theme.accent.to_ascii_lowercase(),
            None => "#76eede".into(),
        }
    }
}

/// "#rrggbb" / "#rgb" -> "R;G;B".
fn parse_hex_rgb(hex: &str) -> Option<String> {
    let h = hex.trim().strip_prefix('#')?;
    let h = match h.len() {
        3 => h.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => h.to_string(),
        _ => return None,
    };
    let n = u32::from_str_radix(&h, 16).ok()?;
    Some(format!(
        "{};{};{}",
        (n >> 16) & 255,
        (n >> 8) & 255,
        n & 255
    ))
}
