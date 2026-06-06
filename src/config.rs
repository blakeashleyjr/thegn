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

/// `[monitor]` — the resource managers opened from the top-bar stats widget
/// (highlight a stat with Super+Alt+Up, then Enter). Each is a shell command
/// run in an embedded tiled pane. `system` backs the CPU and MEM segments; `gpu`
/// backs the GPU segment.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    /// CPU/RAM monitor (default `btm`, ClementTsang/bottom).
    pub system: String,
    /// GPU monitor (default `nvtop`).
    pub gpu: String,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        MonitorConfig {
            system: "btm".into(),
            gpu: "nvtop".into(),
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
    pub monitor: MonitorConfig,
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
            monitor: MonitorConfig::default(),
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

    /// The resource-monitor command for a stat segment: `cpu`/`mem` → the
    /// system monitor, `gpu` → the GPU monitor. Unknown kinds → `None`.
    pub fn monitor_command(&self, kind: &str) -> Option<&str> {
        match kind {
            "cpu" | "mem" => Some(self.monitor.system.as_str()),
            "gpu" => Some(self.monitor.gpu.as_str()),
            _ => None,
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_defaults() {
        let m = MonitorConfig::default();
        assert_eq!(m.system, "btm");
        assert_eq!(m.gpu, "nvtop");
    }

    #[test]
    fn monitor_command_maps_kinds() {
        let cfg = Config::default();
        assert_eq!(cfg.monitor_command("cpu"), Some("btm"));
        assert_eq!(cfg.monitor_command("mem"), Some("btm"));
        assert_eq!(cfg.monitor_command("gpu"), Some("nvtop"));
        assert_eq!(cfg.monitor_command("disk"), None);
        assert_eq!(cfg.monitor_command(""), None);
    }

    #[test]
    fn monitor_command_honors_overrides() {
        let cfg = Config {
            monitor: MonitorConfig {
                system: "htop".into(),
                gpu: "nvitop".into(),
            },
            ..Config::default()
        };
        assert_eq!(cfg.monitor_command("cpu"), Some("htop"));
        assert_eq!(cfg.monitor_command("gpu"), Some("nvitop"));
    }

    #[test]
    fn missing_monitor_table_uses_defaults() {
        // A config.toml without a [monitor] table parses with serde defaults.
        let cfg: Config = toml::from_str("base_branch = \"main\"").unwrap();
        assert_eq!(cfg.monitor.system, "btm");
        assert_eq!(cfg.monitor.gpu, "nvtop");
    }

    #[test]
    fn parses_monitor_table() {
        let cfg: Config =
            toml::from_str("[monitor]\nsystem = \"htop\"\ngpu = \"nvtop\"\n").unwrap();
        assert_eq!(cfg.monitor.system, "htop");
        assert_eq!(cfg.monitor.gpu, "nvtop");
    }

    #[test]
    fn partial_monitor_table_keeps_serde_defaults() {
        // Only one key set — the other falls back to its default.
        let cfg: Config = toml::from_str("[monitor]\ngpu = \"nvitop\"\n").unwrap();
        assert_eq!(cfg.monitor.system, "btm");
        assert_eq!(cfg.monitor.gpu, "nvitop");
    }

    #[test]
    fn parse_hex_rgb_accepts_3_and_6_digit_and_rejects_junk() {
        assert_eq!(parse_hex_rgb("#76eede").as_deref(), Some("118;238;222"));
        assert_eq!(parse_hex_rgb("#fff").as_deref(), Some("255;255;255"));
        assert_eq!(parse_hex_rgb("#000").as_deref(), Some("0;0;0"));
        assert_eq!(parse_hex_rgb("76eede"), None); // requires a leading '#'
        assert_eq!(parse_hex_rgb("#12g456"), None);
        assert_eq!(parse_hex_rgb("#1234"), None);
        assert_eq!(parse_hex_rgb(""), None);
    }

    #[test]
    fn accent_helpers_fall_back_to_teal_on_bad_hex() {
        let good = Config {
            theme: ThemeConfig {
                accent: "#FFffFF".into(),
            },
            ..Config::default()
        };
        assert_eq!(good.accent_rgb(), "255;255;255");
        assert_eq!(good.accent_hex(), "#ffffff"); // normalized to lowercase
        let bad = Config {
            theme: ThemeConfig {
                accent: "not-a-color".into(),
            },
            ..Config::default()
        };
        assert_eq!(bad.accent_hex(), "#76eede");
        assert_eq!(bad.accent_rgb(), crate::theme::TEAL);
    }
}
