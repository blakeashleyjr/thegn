//! Live labels for tabs whose stored names are still their default ordinals.

use crate::{panes::Panes, session::Session};

/// Resolve the active worktree's chip labels from stored names and live pane titles.
pub(crate) fn labels(session: &Session, panes: &Panes, enabled: bool) -> Vec<String> {
    let Some(group) = session.active_group() else {
        return Vec::new();
    };

    group
        .tabs
        .iter()
        .enumerate()
        .map(|(index, tab)| {
            let ordinal = (index + 1).to_string();
            let title = panes
                .table
                .get(&tab.focused_pane)
                .and_then(|pane| pane.emulator().title());
            display_label(&tab.title, &ordinal, title.as_deref(), enabled)
        })
        .collect()
}

fn display_label(
    stored: &str,
    ordinal: &str,
    terminal_title: Option<&str>,
    enabled: bool,
) -> String {
    if enabled && stored == ordinal {
        if let Some(title) = terminal_title.and_then(clean_terminal_title) {
            return title;
        }
    }
    stored.to_owned()
}

/// Remove a leading terminal spinner and reject title strings that are only paths.
fn clean_terminal_title(raw: &str) -> Option<String> {
    let mut title = raw.trim();
    while let Some(first) = title.chars().next()
        && is_spinner(first)
    {
        title = title[first.len_utf8()..].trim_start();
    }
    let title = title.trim();
    if title.is_empty() || is_bare_path(title) {
        None
    } else {
        Some(title.to_owned())
    }
}

fn is_spinner(ch: char) -> bool {
    matches!(
        ch,
        '✳' | '✢'
            | '✽'
            | '✶'
            | '✻'
            | '✺'
            | '✹'
            | '✸'
            | '✷'
            | '⠋'
            | '⠙'
            | '⠹'
            | '⠸'
            | '⠼'
            | '⠴'
            | '⠦'
            | '⠧'
            | '⠇'
            | '⠏'
            | '⠁'
            | '⠂'
            | '⠄'
            | '⡀'
            | '⢀'
            | '⠠'
            | '⠐'
            | '⠈'
    )
}

fn is_bare_path(title: &str) -> bool {
    title.starts_with('/')
        || title.starts_with("~/")
        || title.starts_with("./")
        || title.starts_with("../")
        || title.ends_with('/')
        || title.ends_with('\\')
        || (title.as_bytes().get(1) == Some(&b':')
            && title
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphabetic))
}

#[cfg(test)]
mod tests {
    use super::{clean_terminal_title, display_label};

    #[test]
    fn cleans_spinner_prefix_and_ignores_bare_paths() {
        assert_eq!(
            clean_terminal_title(" ✳  Fix login flow "),
            Some("Fix login flow".into())
        );
        assert_eq!(clean_terminal_title("⠋ /Users/me/project/"), None);
        assert_eq!(clean_terminal_title("  "), None);
    }

    #[test]
    fn custom_tab_name_wins_and_default_uses_terminal_title() {
        assert_eq!(
            display_label("Review", "1", Some("Agent task"), true),
            "Review"
        );
        assert_eq!(
            display_label("1", "1", Some("Agent task"), true),
            "Agent task"
        );
        assert_eq!(display_label("1", "1", Some("/tmp/project"), true), "1");
        assert_eq!(display_label("1", "1", Some("Agent task"), false), "1");
    }
}
