//! The `[[presets]]` config family — named launch configurations that *layer*
//! on the existing program registry rather than duplicating it.
//!
//! A preset is a named launch *shape*: a list of `commands`, a `mode`
//! (`split`/`tabs`), an optional worktree-relative `cwd`, an `env` overlay, and
//! an optional saved-`layout` ref. Each command resolves **first as an exact
//! `[[agents]]`/`[[tools]]` name** (the launch picker's resolution) and
//! otherwise runs via the login shell — so presets never introduce a second
//! program registry, and a referenced agent keeps its own provider/sandbox
//! provisioning semantics.
//!
//! Everything here is **pure** (no I/O, no host types): parsing, validation, the
//! command-classification fold, and the cwd/env overlay computation. The host
//! (`thegn-host/src/handlers/launch.rs`) drives the real launch-spec pipeline
//! with these decisions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Config, NamedCommand, config_enum, config_warn, expand_env_ref};

config_enum! {
    /// How a `[[presets]]` entry opens its programs. `split` (the default) opens
    /// all commands as an even split in **one** new tab; `tabs` opens **one new
    /// tab per command**.
    pub enum PresetMode: "preset mode" {
        Split = "split" | "panes" | "pane",
        Tabs = "tabs" | "tab",
    } default = Split;
}

/// A `[[presets]]` entry — a named, reusable launch shape. Carries no
/// provider/account/provisioning semantics of its own; a command that resolves
/// to an `[[agents]]` entry brings those.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema, Default)]
#[serde(default)]
pub struct Preset {
    /// Stable name; how the preset is summoned (launch menu, `open --preset`,
    /// a `[[worktree_templates]] preset` ref). Unique — the first wins.
    pub name: String,
    /// One-line description shown beside the name in the launch menu.
    pub description: String,
    /// The programs to launch. Each string resolves **first** as an exact
    /// `[[agents]]`/`[[tools]]` name, else runs via the login shell (`""` and
    /// `"shell"` are the plain login shell). Ignored when `layout` is set.
    pub commands: Vec<String>,
    /// `split` (default): the commands as an even split in one new tab; `tabs`:
    /// one new tab per command.
    pub mode: PresetMode,
    /// Working directory, relative to the worktree root (default: the root).
    pub cwd: Option<String>,
    /// Per-pane environment overlay, applied last. Values expand through the
    /// secret indirection (`env:VAR` / `file:PATH`) — never store raw secrets.
    pub env: BTreeMap<String, String>,
    /// A saved named layout to apply instead of `commands` (takes precedence,
    /// matching `[[worktree_templates]]`). An unknown name warns and falls back
    /// to `commands` at apply time.
    pub layout: Option<String>,
}

/// How one preset command resolves against the program registry — the pure
/// classification the host maps onto the launch-spec pipeline (a `Named` entry
/// launches by name with its full agent/tool semantics; a `ShellCommand` runs
/// raw via the login shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresetCommand {
    /// An exact `[[agents]]`/`[[tools]]` entry, launched by name.
    Named(String),
    /// The plain login shell (an empty string or the `shell` sentinel).
    Shell,
    /// A raw command line, run via the login shell.
    ShellCommand(String),
}

/// Classify a single preset command: an exact `[[agents]]`/`[[tools]]` name
/// wins (the picker's resolution); the empty string and `shell` are the login
/// shell; anything else is a raw shell command.
pub fn classify_command(
    cmd: &str,
    agents: &[NamedCommand],
    tools: &[NamedCommand],
) -> PresetCommand {
    let c = cmd.trim();
    if c.is_empty() || c == "shell" {
        return PresetCommand::Shell;
    }
    if agents.iter().any(|a| a.name == c) || tools.iter().any(|t| t.name == c) {
        return PresetCommand::Named(c.to_string());
    }
    PresetCommand::ShellCommand(c.to_string())
}

impl Preset {
    /// Whether this preset applies a saved named layout (non-empty `layout`)
    /// rather than its `commands`.
    pub fn uses_layout(&self) -> bool {
        self.layout
            .as_deref()
            .map(str::trim)
            .is_some_and(|l| !l.is_empty())
    }

    /// The saved-layout name, if set and non-empty.
    pub fn layout_name(&self) -> Option<&str> {
        self.layout
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
    }

    /// Classify every command against the registries, in order. Empty when the
    /// preset uses a layout (the host applies the layout instead).
    pub fn resolved_commands(
        &self,
        agents: &[NamedCommand],
        tools: &[NamedCommand],
    ) -> Vec<PresetCommand> {
        self.commands
            .iter()
            .map(|c| classify_command(c, agents, tools))
            .collect()
    }
}

/// The absolute working directory for a preset pane: the worktree-relative
/// `cwd` joined onto the worktree root, or the root itself when unset. Absolute
/// `cwd` values are honored as-is.
pub fn preset_pane_cwd(worktree: &Path, cwd: Option<&str>) -> PathBuf {
    match cwd.map(str::trim).filter(|c| !c.is_empty()) {
        Some(rel) => worktree.join(rel),
        None => worktree.to_path_buf(),
    }
}

/// Resolve a preset's `env` overlay into concrete key/value pairs, expanding
/// each value through the secret indirection ([`expand_env_ref`]) so
/// credentials live in `env:VAR` / `file:PATH` refs rather than raw config. A
/// value that resolves to nothing (missing var / unreadable file / empty) is
/// dropped — the variable is simply not set.
pub fn resolve_env(env: &BTreeMap<String, String>) -> Vec<(String, String)> {
    env.iter()
        .filter_map(|(k, v)| expand_env_ref(v).map(|val| (k.clone(), val)))
        .collect()
}

/// Strict validation for `thegn config validate` (errors only — these fail the
/// command). Warnings (duplicate names, unknown refs) are a separate, softer
/// channel: [`preset_warnings`].
pub fn validate_presets(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    for (i, p) in cfg.presets.iter().enumerate() {
        let label = if p.name.trim().is_empty() {
            format!("presets[{i}]")
        } else {
            format!("presets[{i}] ({:?})", p.name.trim())
        };
        if p.name.trim().is_empty() {
            out.push(format!(
                "presets[{i}].name: required (presets are launched by name)"
            ));
        }
        // Empty commands with no layout: nothing to launch.
        if p.commands.is_empty() && !p.uses_layout() {
            out.push(format!(
                "{label}: has no commands and no layout — nothing to launch"
            ));
        }
    }
    // A template's `preset` ref is exclusive with its `layout`/`commands`.
    for (i, t) in cfg.worktree_templates.iter().enumerate() {
        let preset_ref = t.preset.as_deref().map(str::trim).filter(|p| !p.is_empty());
        if preset_ref.is_some() {
            let has_layout = t
                .layout
                .as_deref()
                .map(str::trim)
                .is_some_and(|l| !l.is_empty());
            let has_commands = !t.commands.is_empty();
            if has_layout || has_commands {
                out.push(format!(
                    "worktree_templates[{i}] ({:?}): preset is exclusive with layout/commands \
                     — set only one",
                    t.name
                ));
            }
        }
    }
    out
}

/// Soft, best-effort warnings surfaced at config load (never block a launch):
/// duplicate preset names (first wins) and a `[[worktree_templates]] preset`
/// ref naming no configured preset. An unknown saved-`layout` ref is checked at
/// apply time (it needs the SQLite layout registry) and falls back to
/// `commands` there.
pub fn preset_warnings(cfg: &Config) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for p in &cfg.presets {
        let n = p.name.trim();
        if n.is_empty() {
            continue;
        }
        if seen.contains(&n) {
            out.push(format!(
                "preset {n:?} is defined more than once; the first wins"
            ));
        } else {
            seen.push(n);
        }
    }
    for t in &cfg.worktree_templates {
        if let Some(pref) = t.preset.as_deref().map(str::trim).filter(|p| !p.is_empty())
            && !cfg.presets.iter().any(|p| p.name.trim() == pref)
        {
            out.push(format!(
                "worktree template {:?} references unknown preset {pref:?}",
                t.name
            ));
        }
    }
    out
}

impl Config {
    /// Look up a preset by name (first wins — duplicate names are warned about,
    /// not an error).
    pub fn preset(&self, name: &str) -> Option<&Preset> {
        let n = name.trim();
        self.presets.iter().find(|p| p.name.trim() == n)
    }

    /// The configured preset names, in declaration order (for candidate lists
    /// and the launch menu).
    pub fn preset_names(&self) -> Vec<String> {
        self.presets
            .iter()
            .map(|p| p.name.trim().to_string())
            .filter(|n| !n.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorktreeTemplate;

    fn named(name: &str) -> NamedCommand {
        NamedCommand {
            name: name.to_string(),
            command: format!("{name} --run"),
            hints: Vec::new(),
            provider: None,
            resume: false,
            route_via_proxy: false,
            model: None,
            env: Default::default(),
            permissions: Vec::new(),
        }
    }

    #[test]
    fn preset_mode_parses_canon_and_aliases_defaults_split() {
        assert_eq!(
            PresetMode::from_str_validated("split"),
            Ok(PresetMode::Split)
        );
        assert_eq!(PresetMode::from_str_validated("tabs"), Ok(PresetMode::Tabs));
        assert_eq!(PresetMode::from_str_validated("TAB"), Ok(PresetMode::Tabs));
        assert_eq!(
            PresetMode::from_str_validated("panes"),
            Ok(PresetMode::Split)
        );
        assert!(PresetMode::from_str_validated("grid").is_err());
        assert_eq!(PresetMode::default(), PresetMode::Split);
        assert_eq!(PresetMode::Split.as_str(), "split");
        assert_eq!(PresetMode::Tabs.to_string(), "tabs");
    }

    #[test]
    fn classify_prefers_registry_then_shell_then_raw() {
        let agents = [named("claude")];
        let tools = [named("lazygit")];
        // Registry names win.
        assert_eq!(
            classify_command("claude", &agents, &tools),
            PresetCommand::Named("claude".into())
        );
        assert_eq!(
            classify_command("lazygit", &agents, &tools),
            PresetCommand::Named("lazygit".into())
        );
        // Empty / shell sentinel → login shell.
        assert_eq!(classify_command("", &agents, &tools), PresetCommand::Shell);
        assert_eq!(
            classify_command("  ", &agents, &tools),
            PresetCommand::Shell
        );
        assert_eq!(
            classify_command("shell", &agents, &tools),
            PresetCommand::Shell
        );
        // Anything else is a raw shell command (trimmed).
        assert_eq!(
            classify_command("  just dev ", &agents, &tools),
            PresetCommand::ShellCommand("just dev".into())
        );
        // A command that merely starts with an agent name is NOT a name match.
        assert_eq!(
            classify_command("claude --resume", &agents, &tools),
            PresetCommand::ShellCommand("claude --resume".into())
        );
    }

    #[test]
    fn resolved_commands_maps_in_order() {
        let agents = [named("claude")];
        let tools: [NamedCommand; 0] = [];
        let p = Preset {
            commands: vec!["claude".into(), "just dev".into(), "".into()],
            ..Default::default()
        };
        assert_eq!(
            p.resolved_commands(&agents, &tools),
            vec![
                PresetCommand::Named("claude".into()),
                PresetCommand::ShellCommand("just dev".into()),
                PresetCommand::Shell,
            ]
        );
    }

    #[test]
    fn pane_cwd_joins_relative_and_defaults_to_root() {
        let wt = Path::new("/work/tree");
        assert_eq!(preset_pane_cwd(wt, None), PathBuf::from("/work/tree"));
        assert_eq!(preset_pane_cwd(wt, Some("")), PathBuf::from("/work/tree"));
        assert_eq!(
            preset_pane_cwd(wt, Some("services/api")),
            PathBuf::from("/work/tree/services/api")
        );
        // Absolute honored as-is (Path::join semantics).
        assert_eq!(preset_pane_cwd(wt, Some("/abs")), PathBuf::from("/abs"));
    }

    #[test]
    fn resolve_env_expands_indirection_and_drops_missing() {
        // SAFETY: single-threaded test env mutation.
        unsafe {
            std::env::set_var("TG_PRESET_TEST_TOK", "s3cret");
            std::env::remove_var("TG_PRESET_TEST_MISSING");
        }
        let mut env = BTreeMap::new();
        env.insert("PLAIN".to_string(), "value".to_string());
        env.insert("TOK".to_string(), "env:TG_PRESET_TEST_TOK".to_string());
        env.insert("GONE".to_string(), "env:TG_PRESET_TEST_MISSING".to_string());
        let resolved = resolve_env(&env);
        // BTreeMap iterates sorted; GONE is dropped.
        assert_eq!(
            resolved,
            vec![
                ("PLAIN".to_string(), "value".to_string()),
                ("TOK".to_string(), "s3cret".to_string()),
            ]
        );
        unsafe {
            std::env::remove_var("TG_PRESET_TEST_TOK");
        }
    }

    #[test]
    fn validate_rejects_empty_preset_and_missing_name() {
        let mut cfg = Config::default();
        cfg.presets.push(Preset {
            name: "empty".into(),
            ..Default::default()
        });
        cfg.presets.push(Preset {
            name: "".into(),
            commands: vec!["just dev".into()],
            ..Default::default()
        });
        let errs = validate_presets(&cfg);
        assert!(
            errs.iter()
                .any(|e| e.contains("empty") && e.contains("nothing to launch")),
            "{errs:?}"
        );
        assert!(
            errs.iter().any(|e| e.contains("name: required")),
            "{errs:?}"
        );
    }

    #[test]
    fn validate_accepts_layout_only_preset() {
        let mut cfg = Config::default();
        cfg.presets.push(Preset {
            name: "ide".into(),
            layout: Some("ide".into()),
            ..Default::default()
        });
        assert!(validate_presets(&cfg).is_empty());
    }

    #[test]
    fn validate_rejects_template_preset_combined_with_layout_or_commands() {
        let mut cfg = Config::default();
        cfg.worktree_templates.push(WorktreeTemplate {
            name: "combo".into(),
            preset: Some("dev".into()),
            commands: vec!["nvim".into()],
            ..Default::default()
        });
        let errs = validate_presets(&cfg);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].contains("exclusive with layout/commands"),
            "{errs:?}"
        );

        // A template referencing only a preset is fine here (unknown-ref is a warning).
        let mut cfg = Config::default();
        cfg.worktree_templates.push(WorktreeTemplate {
            name: "ok".into(),
            preset: Some("dev".into()),
            ..Default::default()
        });
        assert!(validate_presets(&cfg).is_empty());
    }

    #[test]
    fn warnings_flag_duplicates_and_unknown_template_ref() {
        let mut cfg = Config::default();
        cfg.presets.push(Preset {
            name: "dev".into(),
            commands: vec!["nvim".into()],
            ..Default::default()
        });
        cfg.presets.push(Preset {
            name: "dev".into(),
            commands: vec!["htop".into()],
            ..Default::default()
        });
        cfg.worktree_templates.push(WorktreeTemplate {
            name: "t".into(),
            preset: Some("nope".into()),
            ..Default::default()
        });
        let warns = preset_warnings(&cfg);
        assert!(
            warns.iter().any(|w| w.contains("more than once")),
            "{warns:?}"
        );
        assert!(
            warns
                .iter()
                .any(|w| w.contains("unknown preset") && w.contains("nope")),
            "{warns:?}"
        );
        // First wins on lookup.
        assert_eq!(
            cfg.preset("dev").unwrap().commands,
            vec!["nvim".to_string()]
        );
        assert!(cfg.preset("missing").is_none());
        assert_eq!(
            cfg.preset_names(),
            vec!["dev".to_string(), "dev".to_string()]
        );
    }

    #[test]
    fn uses_layout_and_layout_name() {
        let p = Preset {
            layout: Some("  ide  ".into()),
            ..Default::default()
        };
        assert!(p.uses_layout());
        assert_eq!(p.layout_name(), Some("ide"));
        let p = Preset {
            layout: Some("   ".into()),
            ..Default::default()
        };
        assert!(!p.uses_layout());
        assert_eq!(p.layout_name(), None);
    }
}
