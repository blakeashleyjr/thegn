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

/// `[sandbox.remote]` — optionally run a worktree on a remote machine. Empty
/// `host` means local (the default); set it (e.g. `user@devbox`) to enable.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RemoteConfig {
    pub host: String, // "" => local
    pub port: u16,
    pub transport: String,   // "mosh" (preferred interactive) | "ssh"
    pub mode: String,        // "remote" | "local_exec" | "sshfs"
    pub remote_dir: String,  // where remote worktrees live (mode=remote)
    pub forward_agent: bool, // ssh -A so remote git push uses the host agent
}

impl Default for RemoteConfig {
    fn default() -> Self {
        RemoteConfig {
            host: String::new(),
            port: 22,
            transport: "mosh".into(),
            mode: "remote".into(),
            remote_dir: "~/superzej-worktrees".into(),
            forward_agent: true,
        }
    }
}

impl RemoteConfig {
    /// Whether a remote host is configured (otherwise everything is local).
    pub fn is_remote(&self) -> bool {
        !self.host.trim().is_empty()
    }
}

/// `[sandbox]` — containerize/sandbox a worktree's interactive process. On by
/// default; `backend = "auto"` walks `backend_chain` and falls back to the host
/// shell (with a warning) when nothing is available.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub backend: String, // auto|podman|docker|bwrap|systemd|apple|wsl|none
    pub backend_chain: Vec<String>, // auto detection order; "none" = host fallback
    pub image: String,   // "" => host-toolchain mode
    pub network: String, // nat|host|none
    pub env_passthrough: Vec<String>,
    pub mounts: Vec<String>, // extra binds ("host:dest" or "host"); ":ro" suffix allowed
    pub init_script: String, // runs inside before the agent/shell
    pub devenv: bool,        // wrap inner cmd with `devenv shell --`
    pub on_missing: String,  // warn|prompt|fail
    pub remote: RemoteConfig,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig {
            enabled: true,
            backend: "auto".into(),
            backend_chain: ["podman", "docker", "bwrap", "none"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            image: String::new(),
            network: "nat".into(),
            env_passthrough: [
                "SSH_AUTH_SOCK",
                "GH_TOKEN",
                "GITHUB_TOKEN",
                "ANTHROPIC_API_KEY",
                "TERM",
                "COLORTERM",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            mounts: vec!["~/.gitconfig:ro".into()],
            init_script: String::new(),
            devenv: false,
            on_missing: "warn".into(),
            remote: RemoteConfig::default(),
        }
    }
}

/// Partial overlay deserialized from a repo-root `.superzej.{toml,yaml,yml,json}`
/// — only the keys present override the global `[sandbox]`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SandboxOverlay {
    pub enabled: Option<bool>,
    pub backend: Option<String>,
    pub backend_chain: Option<Vec<String>>,
    pub image: Option<String>,
    pub network: Option<String>,
    pub env_passthrough: Option<Vec<String>>,
    pub mounts: Option<Vec<String>>,
    pub init_script: Option<String>,
    pub devenv: Option<bool>,
    pub on_missing: Option<String>,
    pub remote: Option<RemoteOverlay>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RemoteOverlay {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub transport: Option<String>,
    pub mode: Option<String>,
    pub remote_dir: Option<String>,
    pub forward_agent: Option<bool>,
}

impl SandboxOverlay {
    fn apply(self, base: &mut SandboxConfig) {
        if let Some(v) = self.enabled {
            base.enabled = v;
        }
        if let Some(v) = self.backend {
            base.backend = v;
        }
        if let Some(v) = self.backend_chain {
            base.backend_chain = v;
        }
        if let Some(v) = self.image {
            base.image = v;
        }
        if let Some(v) = self.network {
            base.network = v;
        }
        if let Some(v) = self.env_passthrough {
            base.env_passthrough = v;
        }
        if let Some(v) = self.mounts {
            base.mounts = v;
        }
        if let Some(v) = self.init_script {
            base.init_script = v;
        }
        if let Some(v) = self.devenv {
            base.devenv = v;
        }
        if let Some(v) = self.on_missing {
            base.on_missing = v;
        }
        if let Some(r) = self.remote {
            r.apply(&mut base.remote);
        }
    }
}

impl RemoteOverlay {
    fn apply(self, base: &mut RemoteConfig) {
        if let Some(v) = self.host {
            base.host = v;
        }
        if let Some(v) = self.port {
            base.port = v;
        }
        if let Some(v) = self.transport {
            base.transport = v;
        }
        if let Some(v) = self.mode {
            base.mode = v;
        }
        if let Some(v) = self.remote_dir {
            base.remote_dir = v;
        }
        if let Some(v) = self.forward_agent {
            base.forward_agent = v;
        }
    }
}

/// The shape of a repo-root `.superzej.*` file: a `[sandbox]` table overlay.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct RepoConfigFile {
    sandbox: SandboxOverlay,
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
    pub sandbox: SandboxConfig,
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
            sandbox: SandboxConfig::default(),
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

    /// The effective sandbox config for a worktree's repo: the global `[sandbox]`
    /// with a repo-root `.superzej.{toml,yaml,yml,json}` overlay applied on top.
    /// Tilde-expands path-bearing fields (mounts, remote_dir).
    pub fn repo_sandbox(&self, repo_root: &std::path::Path) -> SandboxConfig {
        let mut sb = self.sandbox.clone();
        if let Some(overlay) = load_repo_overlay(repo_root) {
            overlay.sandbox.apply(&mut sb);
        }
        sb.mounts = sb
            .mounts
            .iter()
            .map(|m| match m.split_once(':') {
                Some((host, opt)) => format!("{}:{opt}", util::expand_tilde(host)),
                None => util::expand_tilde(m),
            })
            .collect();
        // NB: remote.remote_dir is a *remote* path — its `~` is expanded on the
        // remote host (see new_worktree::create_remote), not against the local HOME.
        sb
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

/// Load and parse a repo-root `.superzej.*` overlay, if present. Tries TOML,
/// YAML, then JSON (first existing file wins); parse errors warn and are ignored
/// so a malformed repo file never blocks opening a worktree.
fn load_repo_overlay(repo_root: &std::path::Path) -> Option<RepoConfigFile> {
    for (ext, kind) in [
        ("toml", "toml"),
        ("yaml", "yaml"),
        ("yml", "yaml"),
        ("json", "json"),
    ] {
        let path = repo_root.join(format!(".superzej.{ext}"));
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed: Result<RepoConfigFile, String> = match kind {
            "toml" => toml::from_str(&text).map_err(|e| e.to_string()),
            "yaml" => serde_yaml::from_str(&text).map_err(|e| e.to_string()),
            _ => serde_json::from_str(&text).map_err(|e| e.to_string()),
        };
        return match parsed {
            Ok(cfg) => Some(cfg),
            Err(e) => {
                crate::msg::warn(&format!("{}: parse error: {e}; ignoring", path.display()));
                None
            }
        };
    }
    None
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

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("sz-cfg-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    // The same overlay expressed in each format must produce identical results,
    // and only the present keys override the global defaults.
    #[test]
    fn repo_overlay_all_three_formats_agree() {
        let cfg = Config::default();
        let cases = [
            (
                "toml",
                ".superzej.toml",
                "[sandbox]\nimage = \"img:1\"\ninit_script = \"echo hi\"\n[sandbox.remote]\nhost = \"user@box\"\n",
            ),
            (
                "yaml",
                ".superzej.yaml",
                "sandbox:\n  image: img:1\n  init_script: echo hi\n  remote:\n    host: user@box\n",
            ),
            (
                "json",
                ".superzej.json",
                "{\"sandbox\":{\"image\":\"img:1\",\"init_script\":\"echo hi\",\"remote\":{\"host\":\"user@box\"}}}",
            ),
        ];
        for (tag, file, body) in cases {
            let dir = tmpdir(tag);
            std::fs::write(dir.join(file), body).unwrap();
            let sb = cfg.repo_sandbox(&dir);
            assert_eq!(sb.image, "img:1", "{tag}: image overridden");
            assert_eq!(sb.init_script, "echo hi", "{tag}: init overridden");
            assert_eq!(sb.remote.host, "user@box", "{tag}: remote host overridden");
            // Untouched keys keep their defaults.
            assert!(sb.enabled, "{tag}: enabled keeps default");
            assert_eq!(sb.backend, "auto", "{tag}: backend keeps default");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn no_repo_file_yields_global() {
        let cfg = Config::default();
        let dir = tmpdir("none");
        let sb = cfg.repo_sandbox(&dir);
        assert_eq!(sb.image, ""); // global default (host-toolchain)
        assert!(!sb.remote.is_remote());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
