//! The generated keybindings help page: the *effective* keymap — builtins,
//! `[keybinds]` rebinds, and custom `[[actions]]` — rendered as markdown at
//! registry-build time, so the page always shows the user's real chords.

use thegn_core::keymap::{self, Context};

fn context_heading(ctx: Context) -> &'static str {
    match ctx {
        Context::Global => "Everywhere",
        Context::Center => "Terminal (center)",
        Context::Left => "Sidebar",
        Context::Right => "Panel",
        Context::Top => "Masthead",
        Context::Bottom => "Status bar / drawer",
        Context::TopAndBottom => "Bars",
    }
}

/// Escape a label for safe embedding in the help markdown subset: backticks
/// would open code spans, `[[` would open links.
fn escape(label: &str) -> String {
    label.replace('`', "'").replace("[[", "[ [")
}

/// Build the full page source (frontmatter + markdown) for `cfg`.
pub fn keybindings_page(cfg: &thegn_core::config::Config) -> String {
    let mut resolved = keymap::effective(cfg);
    resolved.sort_by(|a, b| {
        (std::cmp::Reverse(a.priority), &a.menu_label)
            .cmp(&(std::cmp::Reverse(b.priority), &b.menu_label))
    });

    let mut out = String::from(
        "---\nid: keybindings\ntitle: Keybindings\norder: 35\ngenerated: true\n---\n\n\
         # Keybindings\n\n\
         Your **effective** keymap: built-in defaults, `[keybinds]` rebinds, and \
         custom `[[actions]]`, exactly as they resolve right now. Rebind any row \
         by id in `[keybinds]` — see [[configuration]].\n",
    );

    let order = [
        Context::Global,
        Context::Center,
        Context::Left,
        Context::Right,
        Context::Top,
        Context::Bottom,
        Context::TopAndBottom,
    ];
    for ctx in order {
        let rows: Vec<_> = resolved
            .iter()
            .filter(|r| !r.custom && !r.chords.is_empty() && r.contexts.first() == Some(&ctx))
            .collect();
        if rows.is_empty() {
            continue;
        }
        out.push_str(&format!("\n## {}\n\n", context_heading(ctx)));
        for r in rows {
            let chords: Vec<String> = r
                .chords
                .iter()
                .map(|c| format!("`{}`", c.to_hint()))
                .collect();
            out.push_str(&format!(
                "- {} — {}\n",
                chords.join(" / "),
                escape(&r.menu_label)
            ));
        }
    }

    let palette_only: Vec<_> = resolved
        .iter()
        .filter(|r| !r.custom && r.chords.is_empty())
        .collect();
    if !palette_only.is_empty() {
        out.push_str(
            "\n## Palette-only\n\nNo default chord — run from the [[command-palette]] \
             or bind in `[keybinds]` by id.\n\n",
        );
        for r in palette_only {
            out.push_str(&format!("- {} — `{}`\n", escape(&r.menu_label), r.id));
        }
    }

    let custom: Vec<_> = resolved.iter().filter(|r| r.custom).collect();
    if !custom.is_empty() {
        out.push_str("\n## Your actions\n\nFrom `[[actions]]` in your config.\n\n");
        for r in custom {
            let chords: Vec<String> = r
                .chords
                .iter()
                .map(|c| format!("`{}`", c.to_hint()))
                .collect();
            out.push_str(&format!(
                "- {} — {}\n",
                chords.join(" / "),
                escape(&r.menu_label)
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_page_has_the_essentials() {
        let src = keybindings_page(&thegn_core::config::Config::default());
        assert!(src.contains("`Alt-w`"), "default new-worktree chord");
        assert!(src.contains("New worktree"));
        assert!(src.contains("## Everywhere"));
        assert!(src.contains("## Palette-only"));
        assert!(
            !src.contains("## Your actions"),
            "no custom actions by default"
        );
    }

    #[test]
    fn page_parses_cleanly_through_the_help_model() {
        let src = keybindings_page(&thegn_core::config::Config::default());
        let (meta, body) = thegn_core::help::frontmatter::parse(&src).expect("valid frontmatter");
        assert_eq!(meta.id, "keybindings");
        assert!(meta.generated);
        let blocks = thegn_core::help::markdown::parse(body);
        // Internal links may only point at pages that exist.
        for t in thegn_core::help::markdown::links(&blocks) {
            if let thegn_core::help::LinkTarget::Page(id) = t {
                assert!(
                    ["configuration", "command-palette"].contains(&id.as_str()),
                    "unexpected link target {id}"
                );
            }
        }
    }

    #[test]
    fn rebinds_show_up() {
        let mut cfg = thegn_core::config::Config::default();
        cfg.keybinds
            .insert("new-worktree".to_string(), "Ctrl Alt u".to_string());
        let src = keybindings_page(&cfg);
        assert!(src.contains("`Ctrl-Alt-u`"), "{src}");
        assert!(!src.contains("`Alt-w` — New worktree"));
    }

    #[test]
    fn escape_neutralizes_markup() {
        assert_eq!(escape("run `rm -rf`"), "run 'rm -rf'");
        assert_eq!(escape("open [[x]]"), "open [ [x]]");
    }
}
