//! Deterministic discovery and selection of repo-authored devcontainer files.
//!
//! Selection is deliberately separate from parsing: callers can show every
//! candidate (and an ambiguity) before any repo-authored content is trusted.

use std::path::{Path, PathBuf};

use crate::devcontainer::{self, DevContainer, ParseError};

/// Discoverable devcontainer files, in the precedence/order used by thegn.
pub fn candidates(worktree: &Path) -> Vec<PathBuf> {
    let primary = worktree.join(".devcontainer/devcontainer.json");
    if primary.is_file() {
        return vec![primary];
    }
    let dotfile = worktree.join(".devcontainer.json");
    if dotfile.is_file() {
        return vec![dotfile];
    }

    let dir = worktree.join(".devcontainer");
    let mut variants = std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let config = path.join("devcontainer.json");
            config.is_file().then_some(config)
        })
        .collect::<Vec<_>>();
    variants.sort();
    variants
}

/// The result of selecting and parsing a devcontainer file.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionResult {
    /// All candidates considered, in deterministic order.
    pub candidates: Vec<PathBuf>,
    /// The selected path, or `None` for no file/ambiguity/error.
    pub selected: Option<PathBuf>,
    /// The parsed selected config, when selection and reading succeeded.
    pub config: Option<DevContainer>,
    /// A surfaced selection, read, or parse failure.
    pub error: Option<SelectionError>,
    /// The exact source text that was parsed for the selected config. Host
    /// providers use this to make an immutable handoff instead of reopening a
    /// mutable repository path after trust has been decided.
    pub raw_content: Option<String>,
}

/// Why a devcontainer could not be selected or parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    /// More than one variant exists and the repo did not select one.
    Ambiguous(Vec<PathBuf>),
    /// A selected file could not be read.
    Read { path: PathBuf, error: String },
    /// A selected file was read but is malformed.
    Parse { path: PathBuf, error: ParseError },
    /// A selector named no discovered variant.
    SelectorNotFound {
        selector: String,
        candidates: Vec<PathBuf>,
    },
}

impl std::fmt::Display for SelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ambiguous(paths) => write!(
                f,
                "multiple devcontainer configs found: {}",
                paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Read { path, error } => write!(f, "{}: cannot read: {error}", path.display()),
            Self::Parse { path, error } => write!(f, "{}: {error}", path.display()),
            Self::SelectorNotFound { selector, .. } => {
                write!(f, "devcontainer selector {selector:?} matched no config")
            }
        }
    }
}

impl std::error::Error for SelectionError {}

/// Select, read, and parse a repo devcontainer config.
///
/// The two primary paths retain their precedence. Variant directories are
/// selected by their folder name (or relative config path); multiple variants
/// without a selector are an error rather than a guessed first match.
pub fn select_and_parse(worktree: &Path, selector: Option<&str>) -> SelectionResult {
    let found = candidates(worktree);
    if found.is_empty() {
        return SelectionResult {
            candidates: found,
            selected: None,
            config: None,
            error: None,
            raw_content: None,
        };
    }

    let selected = if found.len() == 1 && !is_variant(worktree, &found[0]) {
        Some(found[0].clone())
    } else if let Some(selector) = selector.map(str::trim).filter(|s| !s.is_empty()) {
        found
            .iter()
            .find(|path| selector_matches(worktree, path, selector))
            .cloned()
    } else if found.len() == 1 {
        Some(found[0].clone())
    } else {
        return SelectionResult {
            candidates: found.clone(),
            selected: None,
            config: None,
            error: Some(SelectionError::Ambiguous(found)),
            raw_content: None,
        };
    };

    let Some(path) = selected else {
        return SelectionResult {
            candidates: found.clone(),
            selected: None,
            config: None,
            error: Some(SelectionError::SelectorNotFound {
                selector: selector.unwrap_or_default().trim().to_string(),
                candidates: found,
            }),
            raw_content: None,
        };
    };
    let read = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return SelectionResult {
                candidates: found,
                selected: Some(path.clone()),
                config: None,
                error: Some(SelectionError::Read {
                    path,
                    error: error.to_string(),
                }),
                raw_content: None,
            };
        }
    };
    match devcontainer::parse(&read) {
        Ok(mut config) => {
            config.config_dir = path.parent().map(Path::to_path_buf);
            config.config_path = Some(path.clone());
            SelectionResult {
                candidates: found,
                selected: Some(path),
                config: Some(config),
                error: None,
                raw_content: Some(read),
            }
        }
        Err(error) => SelectionResult {
            candidates: found,
            selected: Some(path.clone()),
            config: None,
            error: Some(SelectionError::Parse { path, error }),
            raw_content: None,
        },
    }
}

/// Return a config path relative to the repo root, using `/` separators.
pub fn relative_path(worktree: &Path, config: &Path) -> PathBuf {
    config
        .strip_prefix(worktree)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| config.to_path_buf())
}

fn is_variant(worktree: &Path, path: &Path) -> bool {
    relative_path(worktree, path)
        .parent()
        .is_some_and(|parent| parent != Path::new(".devcontainer"))
}

fn selector_matches(worktree: &Path, path: &Path, selector: &str) -> bool {
    let rel = relative_path(worktree, path);
    let rel_display = rel.to_string_lossy();
    let folder = rel
        .parent()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy());
    selector == rel_display
        || selector == rel_display.trim_end_matches("/devcontainer.json")
        || folder.is_some_and(|name| name == selector)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let path = std::env::temp_dir().join(format!("tg-dc-select-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn variants_are_ambiguous_until_selected() {
        let root = temp();
        std::fs::create_dir_all(root.join(".devcontainer/a")).unwrap();
        std::fs::create_dir_all(root.join(".devcontainer/b")).unwrap();
        std::fs::write(
            root.join(".devcontainer/a/devcontainer.json"),
            "{\"image\":\"a\"}",
        )
        .unwrap();
        std::fs::write(
            root.join(".devcontainer/b/devcontainer.json"),
            "{\"image\":\"b\"}",
        )
        .unwrap();
        assert!(matches!(
            select_and_parse(&root, None).error,
            Some(SelectionError::Ambiguous(_))
        ));
        let selected = select_and_parse(&root, Some("b"));
        assert_eq!(
            selected.config.unwrap().source,
            devcontainer::ImageSource::Image("b".into())
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
