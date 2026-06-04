//! `superzej new-tab` — open a SECOND full-chrome tab on the current worktree
//! (Alt+t, and zellij tab-mode `n` is repointed here). The tab is named
//! `{base} ·N` (`{slug}/{branch} ·2`, `·3`, …) so the tabbar lists it next to
//! its worktree; the center pane is a plain shell (worktree-tab-extra layout).
//! No worktree is created and no DB row is written — closing the tab is just
//! closing a tab. The diff/PR panel resolves `·N` tabs by stripping the suffix
//! (see resolve.rs).

use crate::{msg, repo, util, zellij};
use anyhow::Result;

pub fn run() -> Result<()> {
    if !zellij::in_zellij() {
        msg::die("new-tab only works inside the superzej session");
    }
    let cwd = std::env::current_dir()?;
    let Some(top) = repo::toplevel(&cwd) else {
        msg::die("not inside a git repository — open a workspace first");
    };
    let Some(main) = repo::main_worktree(&cwd) else {
        msg::die("could not resolve the repo's main worktree");
    };
    let slug = repo::repo_slug(&main);

    // Base tab name: the tab this worktree already lives in.
    let base = if top == main {
        repo::home_tab(&slug)
    } else {
        let branch = util::git_out(&top, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .unwrap_or_else(|| "detached".into());
        repo::branch_tab(&slug, &branch)
    };

    let name = next_free_name(&base, &zellij::tab_names());
    msg::info(&format!("opening tab {name}"));
    if !zellij::new_tab(&name, &top, Some("worktree-tab-extra")) {
        zellij::new_tab(&name, &top, None);
    }
    Ok(())
}

/// Lowest free `"{base} ·N"` (N ≥ 2) among the existing tab names.
fn next_free_name(base: &str, tabs: &[String]) -> String {
    let mut n: u32 = 2;
    loop {
        let candidate = format!("{base} \u{b7}{n}");
        if !tabs.iter().any(|t| t == &candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Strip a `" ·N"` page suffix off a tab name (the panel resolves extra tabs
/// to the same worktree as their base tab).
pub fn strip_page_suffix(tab: &str) -> &str {
    let Some((base, suffix)) = tab.rsplit_once(" \u{b7}") else {
        return tab;
    };
    if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
        base
    } else {
        tab
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_first_free_page_number() {
        let tabs = vec!["r/b".to_string(), "r/b ·2".to_string()];
        assert_eq!(next_free_name("r/b", &tabs), "r/b ·3");
        assert_eq!(next_free_name("r/x", &tabs), "r/x ·2");
    }

    #[test]
    fn strips_only_real_page_suffixes() {
        assert_eq!(strip_page_suffix("r/b ·2"), "r/b");
        assert_eq!(strip_page_suffix("r/b ·12"), "r/b");
        assert_eq!(strip_page_suffix("r/b"), "r/b");
        assert_eq!(strip_page_suffix("r/b ·x"), "r/b ·x");
        assert_eq!(strip_page_suffix("r/b ·"), "r/b ·");
    }
}
