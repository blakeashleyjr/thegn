//! Resolve a plain session-open prompt from an optional brief file.

use anyhow::{Context, Result};
use std::io::Read;
use std::path::Path;

const MAX_BRIEF_FILE_BYTES: usize = 256 * 1024;

fn truncation_warning(path: &str, rendered_bytes: usize) -> String {
    format!(
        "warning: brief file `{path}` rendered to {rendered_bytes} bytes and was truncated to the {}-byte cap",
        thegn_core::agent_task::MAX_RENDERED_PROMPT_BYTES
    )
}

pub(crate) fn resolve_brief(
    prompt: &str,
    brief_file: Option<&str>,
    raw_vars: &[String],
) -> Result<String> {
    let Some(path) = brief_file else {
        if !raw_vars.is_empty() {
            anyhow::bail!("--vars requires --brief-file");
        }
        return Ok(prompt.to_string());
    };

    let path = Path::new(path);
    let display_path = path.display().to_string();
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("reading brief file `{display_path}`"))?;
    let mut bytes = Vec::new();
    file.take((MAX_BRIEF_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading brief file `{display_path}`"))?;
    if bytes.len() > MAX_BRIEF_FILE_BYTES {
        anyhow::bail!("brief file `{display_path}` exceeds the 256 KiB limit");
    }
    let template = String::from_utf8(bytes)
        .with_context(|| format!("brief file `{display_path}` is not valid UTF-8"))?;
    if template.trim().is_empty() {
        anyhow::bail!("brief file `{display_path}` is empty");
    }

    let vars = raw_vars
        .iter()
        .map(|binding| {
            thegn_core::brief_render::parse_var(binding)
                .map_err(|error| anyhow::anyhow!("invalid --vars `{binding}`: {error}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let rendered = thegn_core::brief_render::render_brief(&template, &vars)
        .map_err(|error| anyhow::anyhow!("brief file `{display_path}`: {error}"))?;
    if rendered.truncated {
        eprintln!(
            "{}",
            truncation_warning(&display_path, rendered.rendered_bytes)
        );
    }
    Ok(rendered.text)
}

#[cfg(test)]
mod tests {
    use super::resolve_brief;
    use std::fs;

    #[test]
    fn missing_file_names_the_path() {
        let error = resolve_brief("", Some("/definitely/missing/brief"), &[]).unwrap_err();
        assert!(error.to_string().contains("/definitely/missing/brief"));
    }

    #[test]
    fn empty_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brief.md");
        fs::write(&path, " \n").unwrap();
        let error = resolve_brief("", path.to_str(), &[]).unwrap_err();
        assert!(error.to_string().contains("empty"));
        assert!(error.to_string().contains("brief.md"));
    }

    #[test]
    fn oversized_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brief.md");
        fs::write(&path, vec![b'x'; 256 * 1024 + 1]).unwrap();
        let error = resolve_brief("", path.to_str(), &[]).unwrap_err();
        assert!(error.to_string().contains("256 KiB"));
        assert!(error.to_string().contains("brief.md"));
    }

    #[test]
    fn file_is_rendered_once_when_vars_are_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brief.md");
        fs::write(&path, "Hello {name}").unwrap();
        let vars = vec!["name=lane".to_string()];
        assert_eq!(
            resolve_brief("unused", path.to_str(), &vars).unwrap(),
            "Hello lane"
        );
    }

    #[test]
    fn no_vars_keeps_braces_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brief.md");
        fs::write(&path, "Rust {code}").unwrap();
        assert_eq!(
            resolve_brief("unused", path.to_str(), &[]).unwrap(),
            "Rust {code}"
        );
    }

    #[test]
    fn non_utf8_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("brief.md");
        fs::write(&path, [0xff, 0xfe]).unwrap();
        let error = resolve_brief("", path.to_str(), &[]).unwrap_err();
        assert!(error.to_string().contains("UTF-8"));
        assert!(error.to_string().contains("brief.md"));
    }

    #[test]
    fn vars_without_a_file_are_refused() {
        let error = resolve_brief("prompt", None, &["name=x".to_string()]).unwrap_err();
        assert!(error.to_string().contains("--brief-file"));
    }

    #[test]
    fn brief_file_conflicts_with_prompt_and_issue() {
        use clap::Parser as _;

        assert!(
            crate::Cli::try_parse_from([
                "thegn",
                "session",
                "open",
                "--agent",
                "claude",
                "--worktree",
                "/w",
                "--brief-file",
                "brief.md",
                "--prompt",
                "task",
            ])
            .is_err()
        );
        assert!(
            crate::Cli::try_parse_from([
                "thegn",
                "session",
                "open",
                "--agent",
                "claude",
                "--stage",
                "code",
                "--issue",
                "linear:THE-1",
                "--worktree",
                "/w",
                "--brief-file",
                "brief.md",
            ])
            .is_err()
        );
    }

    #[test]
    fn truncation_warning_names_path_rendered_size_and_cap() {
        let warning = super::truncation_warning("brief.md", 70_000);
        assert!(warning.contains("brief.md"));
        assert!(warning.contains("70,000") || warning.contains("70000"));
        assert!(warning.contains("65536"));
    }
}
