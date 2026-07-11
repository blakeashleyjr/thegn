//! Layout import — read `tmuxinator` (YAML) and `sesh` (TOML) project files
//! into a neutral [`ImportedLayout`], so users migrating from tmux bring their
//! layouts without hand-rebuilding them. Read-only: the source files are never
//! modified. Pure parsing + a small discovery walk over the conventional
//! config locations (`<config>/tmuxinator/*.yml`, `<config>/sesh/sesh.toml`);
//! callers surface the results as worktree-template/layout sources.

use anyhow::{Context, Result, bail};
use std::path::Path;

/// One window of an imported layout: a name plus the optional cwd/command it
/// was declared with. Missing fields default to "a plain shell in the layout
/// root".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedWindow {
    pub name: String,
    pub cwd: Option<String>,
    pub command: Option<String>,
}

/// A neutral layout description parsed from a tmuxinator/sesh project file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedLayout {
    pub name: String,
    /// Project root (may be empty when the source file omits it).
    pub root: String,
    pub windows: Vec<ImportedWindow>,
}

impl ImportedLayout {
    /// Offer this import as a new-worktree template: each window's command
    /// becomes one pane of the `commands` even-split (an empty command is a
    /// plain shell), riding the existing `[[worktree_templates]]` apply path.
    pub fn to_worktree_template(&self) -> crate::config::WorktreeTemplate {
        crate::config::WorktreeTemplate {
            name: self.name.clone(),
            commands: self
                .windows
                .iter()
                .map(|w| w.command.clone().unwrap_or_default())
                .collect(),
            ..Default::default()
        }
    }
}

/// Parse a tmuxinator project file (YAML): `name`, `root`, and a `windows`
/// list whose entries are `- name: command`, `- name:` (plain shell), or
/// `- name: {root: …, panes: [cmd, …]}` (the first pane's command is taken).
/// Malformed input is an error, never a panic.
pub fn parse_tmuxinator(src: &str) -> Result<ImportedLayout> {
    let doc: serde_yaml::Value =
        serde_yaml::from_str(src).context("not valid YAML (tmuxinator project)")?;
    let map = doc
        .as_mapping()
        .context("tmuxinator project is not a YAML mapping")?;
    let str_of = |key: &str| -> Option<String> {
        map.get(serde_yaml::Value::from(key))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    };
    let name = str_of("name").context("tmuxinator project has no `name`")?;
    let root = str_of("root").unwrap_or_default();

    let mut windows = Vec::new();
    if let Some(list) = map
        .get(serde_yaml::Value::from("windows"))
        .and_then(|v| v.as_sequence())
    {
        for entry in list {
            let Some(win) = entry.as_mapping().and_then(|m| m.iter().next()) else {
                bail!("tmuxinator window entry is not a `name: …` mapping");
            };
            let (key, val) = win;
            let name = key
                .as_str()
                .context("window name is not a string")?
                .to_string();
            let (cwd, command) = match val {
                serde_yaml::Value::Null => (None, None),
                serde_yaml::Value::String(cmd) => (None, Some(cmd.clone())),
                serde_yaml::Value::Mapping(m) => {
                    let cwd = m
                        .get(serde_yaml::Value::from("root"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let command = m
                        .get(serde_yaml::Value::from("panes"))
                        .and_then(|v| v.as_sequence())
                        .and_then(|panes| panes.first())
                        .and_then(|p| p.as_str())
                        .map(|s| s.to_string());
                    (cwd, command)
                }
                other => bail!("unsupported tmuxinator window value: {other:?}"),
            };
            windows.push(ImportedWindow { name, cwd, command });
        }
    }
    Ok(ImportedLayout {
        name,
        root,
        windows,
    })
}

/// Parse a sesh config (TOML): every `[[session]]` (name/path/startup_command)
/// becomes one single-window layout. Missing optional fields are defaulted;
/// malformed input is an error, never a panic.
pub fn parse_sesh(src: &str) -> Result<Vec<ImportedLayout>> {
    let doc: toml::Value = toml::from_str(src).context("not valid TOML (sesh config)")?;
    let Some(sessions) = doc.get("session").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for s in sessions {
        let str_of = |key: &str| s.get(key).and_then(|v| v.as_str()).map(|v| v.to_string());
        let name = str_of("name").context("sesh [[session]] has no `name`")?;
        let path = str_of("path").unwrap_or_default();
        let command = str_of("startup_command");
        out.push(ImportedLayout {
            name: name.clone(),
            root: path.clone(),
            windows: vec![ImportedWindow {
                name,
                cwd: (!path.is_empty()).then_some(path),
                command,
            }],
        });
    }
    Ok(out)
}

/// Parse `src` as whichever format `path`'s extension declares: `.yml`/`.yaml`
/// → one tmuxinator project, `.toml` → sesh sessions. Unknown extensions are
/// an error (not a guess).
pub fn parse_any(path: &Path, src: &str) -> Result<Vec<ImportedLayout>> {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "yml" | "yaml" => Ok(vec![parse_tmuxinator(src)?]),
        "toml" => parse_sesh(src),
        other => bail!(
            "unsupported layout file {} (extension {other:?}; expected .yml/.yaml/.toml)",
            path.display()
        ),
    }
}

/// Discover importable layouts under the conventional config locations:
/// `<config_home>/tmuxinator/*.yml|*.yaml` and `<config_home>/sesh/sesh.toml`.
/// Best-effort: unreadable or malformed files are skipped (an import offer
/// must never break the new-worktree flow). Sorted by name for a stable list.
pub fn discover_layouts(config_home: &Path) -> Vec<ImportedLayout> {
    let mut out: Vec<ImportedLayout> = Vec::new();
    let tmuxinator_dir = config_home.join("tmuxinator");
    if let Ok(entries) = std::fs::read_dir(&tmuxinator_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if ext != "yml" && ext != "yaml" {
                continue;
            }
            if let Ok(src) = std::fs::read_to_string(&path)
                && let Ok(layout) = parse_tmuxinator(&src)
            {
                out.push(layout);
            }
        }
    }
    let sesh_file = config_home.join("sesh/sesh.toml");
    if let Ok(src) = std::fs::read_to_string(&sesh_file)
        && let Ok(layouts) = parse_sesh(&src)
    {
        out.extend(layouts);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TMUXINATOR: &str = r#"
name: widget
root: ~/code/widget
windows:
  - editor: nvim
  - server:
      root: ~/code/widget/api
      panes:
        - cargo run
        - htop
  - shell:
"#;

    #[test]
    fn tmuxinator_project_parses() {
        let l = parse_tmuxinator(TMUXINATOR).unwrap();
        assert_eq!(l.name, "widget");
        assert_eq!(l.root, "~/code/widget");
        assert_eq!(
            l.windows,
            vec![
                ImportedWindow {
                    name: "editor".into(),
                    cwd: None,
                    command: Some("nvim".into()),
                },
                ImportedWindow {
                    name: "server".into(),
                    cwd: Some("~/code/widget/api".into()),
                    command: Some("cargo run".into()),
                },
                ImportedWindow {
                    name: "shell".into(),
                    cwd: None,
                    command: None,
                },
            ]
        );
    }

    #[test]
    fn tmuxinator_missing_optional_fields_default() {
        // No root, no windows: still a valid (empty) project.
        let l = parse_tmuxinator("name: bare\n").unwrap();
        assert_eq!(l.name, "bare");
        assert_eq!(l.root, "");
        assert!(l.windows.is_empty());
        // But a missing name is an error.
        assert!(parse_tmuxinator("root: /x\n").is_err());
    }

    #[test]
    fn sesh_sessions_parse() {
        let src = r#"
[[session]]
name = "dotfiles"
path = "~/dotfiles"
startup_command = "nvim"

[[session]]
name = "scratch"
"#;
        let ls = parse_sesh(src).unwrap();
        assert_eq!(ls.len(), 2);
        assert_eq!(ls[0].name, "dotfiles");
        assert_eq!(ls[0].root, "~/dotfiles");
        assert_eq!(ls[0].windows[0].command.as_deref(), Some("nvim"));
        // Missing optional fields defaulted.
        assert_eq!(ls[1].root, "");
        assert_eq!(ls[1].windows[0].cwd, None);
        assert_eq!(ls[1].windows[0].command, None);
        // No [[session]] at all: empty, not an error.
        assert!(parse_sesh("x = 1\n").unwrap().is_empty());
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        assert!(parse_tmuxinator("{unclosed").is_err());
        assert!(parse_tmuxinator("- just\n- a list\n").is_err());
        assert!(parse_tmuxinator("name: x\nwindows:\n  - 42\n").is_err());
        assert!(parse_sesh("[[session]\nname=").is_err());
        assert!(parse_any(Path::new("proj.conf"), "").is_err());
    }

    #[test]
    fn parse_any_dispatches_by_extension() {
        let y = parse_any(Path::new("w.yml"), "name: w\n").unwrap();
        assert_eq!(y[0].name, "w");
        let t = parse_any(Path::new("sesh.toml"), "[[session]]\nname = \"s\"\n").unwrap();
        assert_eq!(t[0].name, "s");
    }

    #[test]
    fn to_worktree_template_maps_window_commands() {
        let l = parse_tmuxinator(TMUXINATOR).unwrap();
        let t = l.to_worktree_template();
        assert_eq!(t.name, "widget");
        assert_eq!(t.commands, vec!["nvim", "cargo run", ""]);
        assert!(
            t.layout.is_none(),
            "imports use commands, not named layouts"
        );
    }

    #[test]
    fn discover_layouts_walks_conventional_locations() {
        let home = std::env::temp_dir().join(format!("tg-layouts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("tmuxinator")).unwrap();
        std::fs::create_dir_all(home.join("sesh")).unwrap();
        std::fs::write(home.join("tmuxinator/widget.yml"), TMUXINATOR).unwrap();
        std::fs::write(home.join("tmuxinator/broken.yml"), "{nope").unwrap();
        std::fs::write(home.join("tmuxinator/ignored.txt"), "name: no").unwrap();
        std::fs::write(
            home.join("sesh/sesh.toml"),
            "[[session]]\nname = \"dot\"\npath = \"~/d\"\n",
        )
        .unwrap();
        let found = discover_layouts(&home);
        assert_eq!(
            found.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
            vec!["dot", "widget"],
            "sorted; broken + non-yaml files skipped"
        );
        // A missing config home is just an empty offer.
        assert!(discover_layouts(&home.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }
}
