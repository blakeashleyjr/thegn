//! Deterministic discovery and selection of repo-authored devcontainer files.
//!
//! Selection is deliberately separate from parsing: callers can show every
//! candidate (and an ambiguity) before any repo-authored content is trusted.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::devcontainer::{self, DevContainer, ParseError};

const MAX_CANDIDATES: usize = 64;
const MAX_DIRECTORY_ENTRIES: usize = 4096;
const MAX_PATH_BYTES: usize = 4096;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Discoverable devcontainer files, in the precedence/order used by thegn.
pub fn candidates(worktree: &Path) -> Vec<PathBuf> {
    candidates_bounded(worktree).unwrap_or_default()
}

fn candidates_bounded(worktree: &Path) -> Result<Vec<PathBuf>, (PathBuf, String)> {
    let devcontainer_dir = worktree.join(".devcontainer");
    if std::fs::symlink_metadata(&devcontainer_dir).is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err((devcontainer_dir, ".devcontainer is a symlink".into()));
    }
    let primary = worktree.join(".devcontainer/devcontainer.json");
    if std::fs::symlink_metadata(&primary).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err((primary, "primary devcontainer config is a symlink".into()));
    }
    if safe_candidate(worktree, &primary) {
        if path_bytes(&primary) > MAX_PATH_BYTES {
            return Err((primary, "devcontainer candidate path exceeds limit".into()));
        }
        return Ok(vec![primary]);
    }
    let dotfile = worktree.join(".devcontainer.json");
    if std::fs::symlink_metadata(&dotfile).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err((dotfile, ".devcontainer.json is a symlink".into()));
    }
    if safe_candidate(worktree, &dotfile) {
        if path_bytes(&dotfile) > MAX_PATH_BYTES {
            return Err((dotfile, "devcontainer candidate path exceeds limit".into()));
        }
        return Ok(vec![dotfile]);
    }

    let dir = worktree.join(".devcontainer");
    if !real_worktree_directory(worktree) || !real_directory(&dir) {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(&dir).map_err(|_| {
        (
            dir.clone(),
            "cannot inspect devcontainer directory".to_string(),
        )
    })?;
    let mut variants = Vec::new();
    for (index, entry) in entries.take(MAX_DIRECTORY_ENTRIES + 1).enumerate() {
        if index == MAX_DIRECTORY_ENTRIES {
            return Err((
                dir.clone(),
                "too many devcontainer directory entries".into(),
            ));
        }
        let entry =
            entry.map_err(|_| (dir.clone(), "cannot inspect devcontainer entry".to_string()))?;
        let path = entry.path();
        let config = path.join("devcontainer.json");
        if entry.file_type().is_ok_and(|kind| kind.is_symlink()) {
            if std::fs::symlink_metadata(&config)
                .is_ok_and(|meta| meta.is_file() || meta.file_type().is_symlink())
            {
                return Err((config, "devcontainer variant directory is a symlink".into()));
            }
            continue;
        }
        if std::fs::symlink_metadata(&config).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err((config, "variant devcontainer config is a symlink".into()));
        }
        if safe_candidate(worktree, &config) {
            if path_bytes(&config) > MAX_PATH_BYTES {
                return Err((config, "devcontainer candidate path exceeds limit".into()));
            }
            variants.push(config);
            if variants.len() > MAX_CANDIDATES {
                return Err((
                    variants[MAX_CANDIDATES].clone(),
                    "too many devcontainer candidates".into(),
                ));
            }
        }
    }
    variants.sort();
    Ok(variants)
}

fn path_bytes(path: &Path) -> usize {
    path.as_os_str().as_encoded_bytes().len()
}

fn real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
}

fn real_worktree_directory(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| meta.is_dir()) && path.canonicalize().is_ok()
}

fn safe_candidate(worktree: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(worktree) else {
        return false;
    };
    if relative
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
        return false;
    }
    if !real_worktree_directory(worktree) {
        return false;
    }
    let parent = path.parent().unwrap_or(worktree);
    if parent != worktree && !real_directory(parent) {
        return false;
    }
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
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
    /// Discovery exceeded a safety bound or could not inspect its directory.
    Discovery { error: String },
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
            Self::Discovery { error } => write!(f, "devcontainer discovery refused: {error}"),
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
    let found = match candidates_bounded(worktree) {
        Ok(found) => found,
        Err((path, error)) => {
            return SelectionResult {
                candidates: vec![path],
                selected: None,
                config: None,
                error: Some(SelectionError::Discovery { error }),
                raw_content: None,
            };
        }
    };
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
    let read = match read_config(worktree, &path) {
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

fn read_config(worktree: &Path, path: &Path) -> std::io::Result<String> {
    if !safe_candidate(worktree, path) || !contained_parent(worktree, path) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "unsafe devcontainer path",
        ));
    }
    let mut file = crate::fsperm::open_regular_file_nofollow(path)?;
    let before = file.metadata()?;
    let opened_handle = same_file::Handle::from_file(file.try_clone()?)?;
    let parent = path.parent().unwrap_or(worktree);
    let parent_before = same_file::Handle::from_path(parent)?;
    if before.len() > MAX_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "devcontainer config exceeds 1 MiB limit",
        ));
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    file.by_ref()
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let current = std::fs::symlink_metadata(path)?;
    let current_handle = same_file::Handle::from_path(path)?;
    let parent_after = same_file::Handle::from_path(parent)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES
        || !current.is_file()
        || current.file_type().is_symlink()
        || before.len() != after.len()
        || after.len() != bytes.len() as u64
        || opened_handle != current_handle
        || parent_before != parent_after
        || !contained_parent(worktree, path)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "devcontainer config changed during read or is oversized",
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "devcontainer config is not UTF-8",
        )
    })
}

fn contained_parent(worktree: &Path, path: &Path) -> bool {
    let (Ok(root), Some(parent)) = (worktree.canonicalize(), path.parent()) else {
        return false;
    };
    parent
        .canonicalize()
        .is_ok_and(|resolved| resolved.starts_with(root))
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
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "tg-dc-select-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
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

    #[cfg(unix)]
    #[test]
    fn symlinked_config_is_not_discovered_or_read() {
        use std::os::unix::fs::symlink;
        let root = temp();
        let outside = root.with_extension("outside");
        std::fs::write(&outside, r#"{"image":"must-not-be-read"}"#).unwrap();
        std::fs::create_dir_all(root.join(".devcontainer")).unwrap();
        symlink(&outside, root.join(".devcontainer/devcontainer.json")).unwrap();
        let result = select_and_parse(&root, None);
        assert!(result.config.is_none());
        assert!(result.raw_content.is_none());
        assert!(result.selected.is_none());
        assert!(matches!(
            result.error,
            Some(SelectionError::Discovery { .. })
        ));
        assert_eq!(
            result.candidates,
            vec![root.join(".devcontainer/devcontainer.json")]
        );
        assert!(matches!(
            devcontainer::detect_and_parse(&root),
            Some(Err(_))
        ));
        std::fs::remove_file(root.join(".devcontainer/devcontainer.json")).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(outside).unwrap();
    }

    #[test]
    fn oversized_selected_config_is_refused_before_parsing() {
        let root = temp();
        std::fs::create_dir_all(root.join(".devcontainer")).unwrap();
        let path = root.join(".devcontainer/devcontainer.json");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_CONFIG_BYTES + 1).unwrap();
        let result = select_and_parse(&root, None);
        assert!(result.config.is_none());
        assert!(matches!(result.error, Some(SelectionError::Read { .. })));
        assert!(result.raw_content.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_variant_directory_is_not_followed() {
        use std::os::unix::fs::symlink;
        let root = temp();
        let outside = root.with_extension("outside-dir");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("devcontainer.json"), r#"{"image":"outside"}"#).unwrap();
        std::fs::create_dir_all(root.join(".devcontainer")).unwrap();
        symlink(&outside, root.join(".devcontainer/variant")).unwrap();
        let result = select_and_parse(&root, None);
        assert!(result.config.is_none());
        assert!(result.raw_content.is_none());
        assert_eq!(
            result.candidates,
            vec![root.join(".devcontainer/variant/devcontainer.json")]
        );
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_variant_config_is_refused() {
        use std::os::unix::fs::symlink;
        let root = temp();
        let outside = root.with_extension("outside-config");
        std::fs::write(&outside, r#"{"image":"outside"}"#).unwrap();
        let variant = root.join(".devcontainer/variant");
        std::fs::create_dir_all(&variant).unwrap();
        symlink(&outside, variant.join("devcontainer.json")).unwrap();

        let result = select_and_parse(&root, None);
        assert!(matches!(
            result.error,
            Some(SelectionError::Discovery { .. })
        ));
        assert_eq!(result.candidates, vec![variant.join("devcontainer.json")]);

        std::fs::remove_file(variant.join("devcontainer.json")).unwrap();
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(outside).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_non_variant_file_does_not_hide_real_variant() {
        use std::os::unix::fs::symlink;
        let root = temp();
        let outside = root.with_extension("outside-file");
        std::fs::write(&outside, "not a variant").unwrap();
        std::fs::create_dir_all(root.join(".devcontainer/real")).unwrap();
        std::fs::write(
            root.join(".devcontainer/real/devcontainer.json"),
            r#"{"image":"real"}"#,
        )
        .unwrap();
        symlink(&outside, root.join(".devcontainer/README")).unwrap();

        let result = select_and_parse(&root, None);
        assert_eq!(
            result.config.unwrap().source,
            devcontainer::ImageSource::Image("real".into())
        );

        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(outside).unwrap();
    }

    #[test]
    fn too_many_variants_are_refused_with_a_candidate() {
        let root = temp();
        for index in 0..=MAX_CANDIDATES {
            let dir = root.join(format!(".devcontainer/variant-{index}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("devcontainer.json"), "{}").unwrap();
        }
        let result = select_and_parse(&root, None);
        assert!(matches!(
            result.error,
            Some(SelectionError::Discovery { .. })
        ));
        assert_eq!(result.candidates.len(), 1);
        assert!(result.candidates[0].ends_with("devcontainer.json"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn discovery_error_has_user_visible_display() {
        let error = SelectionError::Discovery {
            error: "test refusal".into(),
        };
        assert_eq!(
            error.to_string(),
            "devcontainer discovery refused: test refusal"
        );
    }

    #[test]
    fn non_utf8_selected_config_is_refused() {
        let root = temp();
        std::fs::create_dir_all(root.join(".devcontainer")).unwrap();
        std::fs::write(root.join(".devcontainer/devcontainer.json"), [0xff, 0xfe]).unwrap();
        let result = select_and_parse(&root, None);
        assert!(matches!(result.error, Some(SelectionError::Read { .. })));
        assert!(result.raw_content.is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_worktree_root_is_discovered_by_canonical_location() {
        use std::os::unix::fs::symlink;
        let real = temp();
        let alias = real.with_extension("alias");
        std::fs::create_dir_all(real.join(".devcontainer")).unwrap();
        std::fs::write(
            real.join(".devcontainer/devcontainer.json"),
            r#"{"image":"ok"}"#,
        )
        .unwrap();
        symlink(&real, &alias).unwrap();

        assert!(select_and_parse(&alias, None).config.is_some());

        std::fs::remove_file(&alias).unwrap();
        std::fs::remove_dir_all(real).unwrap();
    }
}
