//! Shared selection and presentation helpers for worktree isolation profiles.

use anyhow::Result;
use std::path::Path;
use thegn_core::sandbox_pin::EnvSelection;
use thegn_core::store::WorkspaceStore;

pub(crate) fn selection_for(worktree: &str, repo_root: &Path) -> Result<EnvSelection> {
    let db = thegn_core::db::Db::open()?;
    db.effective_env_selection(worktree, &repo_root.to_string_lossy())
}

pub(crate) fn apply_menu_choice(
    arg: &str,
    worktree: String,
    branch: String,
    refresh_tx: tokio::sync::mpsc::UnboundedSender<crate::hydrate::RefreshKind>,
) -> Result<String> {
    let profile = if arg == "clear" {
        None
    } else {
        Some(thegn_core::sandbox_pin::parse_pin(arg)?)
    };
    if let Some(profile) = profile
        && profile == thegn_core::config::SandboxProfile::Open
    {
        anyhow::bail!("only standard and sealed can be pinned for a worktree");
    }
    let saved_worktree = worktree.clone();
    crate::db_task::persist_critical(move |db| {
        if let Err(error) = db.set_worktree_sandbox_profile(&saved_worktree, profile) {
            tracing::warn!(target: "thegn::sandbox", worktree = %saved_worktree, %error, "could not persist worktree isolation profile");
            crate::db_task::alert(&format!(
                "Could not save isolation profile for {saved_worktree}: {error}"
            ));
            return;
        }
        if let Err(error) = refresh_tx.send(crate::hydrate::RefreshKind::Model) {
            tracing::debug!(target: "thegn::hydrate", %error, "model refresh request failed after isolation profile save");
        }
    });
    let value = profile.map_or_else(
        || "inherit".to_string(),
        |profile| profile.as_str().to_string(),
    );
    Ok(format!(
        "Saving isolation profile for {branch}: {value}. Applies to new panes; restart open panes."
    ))
}

#[cfg(test)]
mod tests {
    use super::selection_for;
    use thegn_core::config::SandboxProfile;
    use thegn_core::sandbox_pin::EnvSelection;
    use thegn_core::store::WorkspaceStore;

    #[test]
    fn sandbox_pin_no_launch_site_drops_the_pin() {
        use std::path::{Path, PathBuf};

        fn source_files(root: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(root).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    source_files(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs")
                    && !path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .ends_with("_tests.rs")
                {
                    out.push(path);
                }
            }
        }

        fn matching_brace(source: &str, open: usize) -> Option<usize> {
            let bytes = source.as_bytes();
            let mut depth = 1_usize;
            let mut i = open + 1;
            let mut raw_end: Option<Vec<u8>> = None;
            let mut string = false;
            let mut escaped = false;
            let mut line_comment = false;
            let mut block_depth = 0_usize;
            while i < bytes.len() {
                if let Some(end) = &raw_end {
                    if bytes[i..].starts_with(end) {
                        i += end.len();
                        raw_end = None;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                if line_comment {
                    if bytes[i] == b'\n' {
                        line_comment = false;
                    }
                    i += 1;
                    continue;
                }
                if block_depth > 0 {
                    if bytes[i..].starts_with(b"/*") {
                        block_depth += 1;
                        i += 2;
                    } else if bytes[i..].starts_with(b"*/") {
                        block_depth -= 1;
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                if string {
                    if escaped {
                        escaped = false;
                    } else if bytes[i] == b'\\' {
                        escaped = true;
                    } else if bytes[i] == b'"' {
                        string = false;
                    }
                    i += 1;
                    continue;
                }
                if bytes[i..].starts_with(b"//") {
                    line_comment = true;
                    i += 2;
                } else if bytes[i..].starts_with(b"/*") {
                    block_depth = 1;
                    i += 2;
                } else if bytes[i] == b'r' {
                    let mut quote = i + 1;
                    while quote < bytes.len() && bytes[quote] == b'#' {
                        quote += 1;
                    }
                    if quote < bytes.len() && bytes[quote] == b'"' {
                        let mut end = vec![b'"'];
                        end.extend(std::iter::repeat_n(b'#', quote - i - 1));
                        raw_end = Some(end);
                        i = quote + 1;
                    } else {
                        i += 1;
                    }
                } else if bytes[i] == b'"' {
                    string = true;
                    i += 1;
                } else if bytes[i] == b'{' {
                    depth += 1;
                    i += 1;
                } else if bytes[i] == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                    i += 1;
                } else {
                    i += 1;
                }
            }
            None
        }

        fn test_module_ranges(source: &str) -> Vec<(usize, usize)> {
            let module = regex::Regex::new(r"#\[cfg\(test\)\]\s*mod\s+\w+\s*\{").unwrap();
            module
                .find_iter(source)
                .filter_map(|found| {
                    let open = found.end() - 1;
                    matching_brace(source, open).map(|close| (found.start(), close + 1))
                })
                .collect()
        }

        fn enclosing_function(source: &str, at: usize) -> Option<(String, usize)> {
            let function = regex::Regex::new(r"\bfn\s+(\w+)\s*(?:<[^>]*>)?\s*\(").unwrap();
            let found = function.find_iter(&source[..at]).last()?;
            let captures = function.captures(found.as_str())?;
            Some((captures[1].to_string(), found.start()))
        }

        fn selection_argument(arguments: &str, with_approvals: bool) -> &str {
            let arguments = arguments.trim().trim_end_matches(',').trim_end();
            let mut depth = 0_i32;
            let mut quoted = false;
            let mut escaped = false;
            let mut commas = Vec::new();
            for (offset, ch) in arguments.char_indices() {
                if quoted {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        quoted = false;
                    }
                    continue;
                }
                match ch {
                    '"' => quoted = true,
                    '(' | '[' | '{' => depth += 1,
                    ')' | ']' | '}' => depth -= 1,
                    ',' if depth == 0 => commas.push(offset),
                    _ => {}
                }
            }
            let mut starts = vec![0];
            starts.extend(commas.iter().map(|comma| comma + 1));
            let ends = commas
                .iter()
                .copied()
                .chain(std::iter::once(arguments.len()))
                .collect::<Vec<_>>();
            let selected_index = starts
                .len()
                .saturating_sub(if with_approvals { 2 } else { 1 });
            let selected_index = selected_index.min(starts.len().saturating_sub(1));
            arguments[starts[selected_index]..ends[selected_index]].trim()
        }

        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut files = Vec::new();
        source_files(&manifest.join("src"), &mut files);
        let mut allowed = std::collections::HashMap::new();
        for line in std::fs::read_to_string(manifest.join("../../test/sandbox-pin-ratchet.txt"))
            .unwrap()
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
        {
            let (site, reason) = line.split_once("  ").expect("site and reason separated");
            assert!(
                site.contains(':'),
                "allowlist entry needs file:function: {site}"
            );
            assert!(
                !reason.trim().is_empty(),
                "every entry needs a reason: {line}"
            );
            assert!(
                allowed.insert(site.to_string(), false).is_none(),
                "duplicate {site}"
            );
        }

        let resolve = regex::Regex::new(r"resolve_env(?:_with)?\s*\(").unwrap();
        let construction = regex::Regex::new(
            r"EnvSelection::(?:none|named)\s*\(|EnvSelection\s*\{[^}]*profile\s*:\s*None",
        )
        .unwrap();
        let selection_binding = regex::Regex::new(r"let\s+(?:mut\s+)?(\w+)\s*=\s*[^;]*$").unwrap();
        let mut violations = Vec::new();
        for path in files {
            let source = std::fs::read_to_string(&path).unwrap();
            let test_ranges = test_module_ranges(&source);
            let relative = path
                .strip_prefix(manifest.join("src"))
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            for call in resolve.find_iter(&source) {
                if test_ranges
                    .iter()
                    .any(|(start, end)| *start <= call.start() && call.start() < *end)
                {
                    continue;
                }
                let Some((function, function_start)) = enclosing_function(&source, call.start())
                else {
                    continue;
                };
                let open = call.end() - 1;
                let Some(close) = matching_brace_like_paren(&source, open + 1) else {
                    continue;
                };
                let call_args = &source[open + 1..close];
                let selection_arg =
                    selection_argument(call_args, call.as_str().contains("resolve_env_with"));
                let prefix = &source[function_start..call.start()];
                let has_unpinned_selection = construction.is_match(selection_arg)
                    || selection_arg == "None"
                    || selection_arg.starts_with("Some(")
                    || construction.find_iter(prefix).any(|construction| {
                        let before = &prefix[..construction.start()];
                        let name = selection_binding
                            .captures(before)
                            .map(|capture| capture[1].to_string());
                        name.is_some_and(|name| {
                            selection_arg
                                .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
                                .any(|part| part == name)
                        })
                    });
                if !has_unpinned_selection {
                    continue;
                }
                let site = format!("{relative}:{function}");
                if let Some(used) = allowed.get_mut(&site) {
                    *used = true;
                } else {
                    violations.push(site);
                }
            }
        }
        let unused = allowed
            .iter()
            .filter(|(_, used)| !**used)
            .map(|(site, _)| site.clone())
            .collect::<Vec<_>>();
        assert!(
            violations.is_empty(),
            "unclassified pinless worktree resolvers: {violations:#?}"
        );
        assert!(unused.is_empty(), "stale resolver reasons: {unused:#?}");

        fn matching_brace_like_paren(source: &str, open: usize) -> Option<usize> {
            let mut depth = 1_usize;
            let mut quoted = false;
            let mut escaped = false;
            for (offset, ch) in source[open..].char_indices() {
                if quoted {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        quoted = false;
                    }
                    continue;
                }
                match ch {
                    '"' => quoted = true,
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(open + offset);
                        }
                    }
                    _ => {}
                }
            }
            None
        }
    }

    #[test]
    fn sandbox_profile_selection_for_reads_the_stored_pin() {
        let _state = crate::testenv::IsolatedState::new();
        let root = tempfile::tempdir().unwrap();
        let db = thegn_core::db::Db::open().unwrap();
        db.set_worktree_sandbox_profile(
            root.path().to_string_lossy().as_ref(),
            Some(SandboxProfile::Sealed),
        )
        .unwrap();
        let selection = selection_for(root.path().to_string_lossy().as_ref(), root.path()).unwrap();
        assert_eq!(selection.profile, Some(SandboxProfile::Sealed));
    }

    #[test]
    fn sandbox_profile_menu_has_unique_standard_sealed_and_clear_choices() {
        let menu = crate::menu::sandbox_profile_menu(SandboxProfile::Hardened, true);
        let rendered = format!("{menu:?}");
        assert!(rendered.contains("sandbox-profile"));
        assert!(rendered.contains("standard"));
        assert!(!rendered.contains("(default)"));
        assert!(rendered.contains("sealed"));
        assert!(rendered.contains("clear"));
        assert!(rendered.contains("Isolation profile"));
    }

    #[test]
    fn sandbox_profile_wizard_choice_reaches_trusted_resolution_before_registration() {
        let _state = crate::testenv::IsolatedState::new();
        let root = tempfile::tempdir().unwrap();
        let loc = thegn_core::remote::GitLoc::Local(root.path().to_path_buf());
        let selection = EnvSelection::none().with_profile(SandboxProfile::Sealed);
        let trusted = crate::handlers::repo_trust::resolve_env_trusted(
            &thegn_core::config::Config::default(),
            root.path(),
            &loc,
            root.path().to_string_lossy().as_ref(),
            &selection,
        )
        .unwrap();
        assert_eq!(trusted.environment.sandbox.profile, SandboxProfile::Sealed);
    }
}
