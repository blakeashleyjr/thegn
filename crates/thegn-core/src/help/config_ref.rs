//! The generated config-reference help page.
//!
//! `config/config.toml.example` documents every key inline and is already
//! embedded in the binary; regenerating the reference from it at runtime
//! means the help page *cannot* drift from the shipped example. The
//! transform is structural, not semantic: each `[table]` becomes a section
//! whose prose is the comment block immediately above the header, and the
//! table's keys (with their inline comments) become a fenced `toml` block.

/// The page id the generated reference is registered under.
pub const PAGE_ID: &str = "config-reference";

struct Section {
    /// `[table]` header line, or "General" for pre-table top-level keys.
    title: String,
    /// Comment block above the header, `#`-stripped.
    prose: Vec<String>,
    /// Verbatim TOML (keys + their comments) for the fence.
    body: Vec<String>,
}

/// Wrap `[[...]]` occurrences in backticks so prose that *mentions* an
/// array-of-tables header doesn't parse as an internal help link.
fn escape_wikilinks(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(start) = rest.find("[[") {
        if let Some(end) = rest[start..].find("]]") {
            out.push_str(&rest[..start]);
            out.push('`');
            out.push_str(&rest[start..start + end + 2]);
            out.push('`');
            rest = &rest[start + end + 2..];
        } else {
            break;
        }
    }
    out.push_str(rest);
    out
}

fn strip_hash(line: &str) -> &str {
    let t = line.trim_start().trim_start_matches('#');
    t.strip_prefix(' ').unwrap_or(t)
}

fn push_body_blank(body: &mut Vec<String>) {
    if body.last().is_some_and(|l| !l.is_empty()) {
        body.push(String::new());
    }
}

/// Generate the full page source (frontmatter + markdown) from the example
/// config. The output parses cleanly through this module's own frontmatter
/// and markdown parsers — the host's ratchet test asserts that.
pub fn page(example: &str) -> String {
    let mut lines = example.lines().map(str::trim_end).peekable();

    // The file's leading comment block (up to the first blank) is the intro.
    let mut intro: Vec<String> = Vec::new();
    while let Some(&line) = lines.peek() {
        if line.trim_start().starts_with('#') {
            intro.push(strip_hash(line).to_string());
            lines.next();
        } else {
            break;
        }
    }

    let mut sections = vec![Section {
        title: "General".to_string(),
        prose: Vec::new(),
        body: Vec::new(),
    }];
    let mut pending: Vec<String> = Vec::new();

    for line in lines {
        let t = line.trim_start();
        if t.starts_with('#') {
            pending.push(line.to_string());
            continue;
        }
        if t.starts_with('[') {
            // A comment block directly above a table header is its prose.
            let prose = pending
                .drain(..)
                .map(|c| strip_hash(&c).to_string())
                .collect();
            sections.push(Section {
                title: t.to_string(),
                prose,
                body: vec![line.to_string()],
            });
            continue;
        }
        let cur = sections.last_mut().expect("sections starts non-empty");
        if t.is_empty() {
            // Blank: any pending comments belong to the fence, verbatim.
            cur.body.append(&mut pending);
            push_body_blank(&mut cur.body);
            continue;
        }
        // A key line: preceding comments document it — keep them in the fence.
        cur.body.append(&mut pending);
        cur.body.push(line.to_string());
    }
    if let Some(cur) = sections.last_mut() {
        cur.body.append(&mut pending);
    }

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("id: {PAGE_ID}\n"));
    out.push_str("title: Config reference\n");
    out.push_str("parent: configuration\n");
    out.push_str("order: 99\n");
    out.push_str("generated: true\n");
    out.push_str("---\n\n# Config reference\n\n");
    out.push_str(
        "Generated from the built-in `config.toml.example` — every key with its default \
         and inline documentation. Copy only the keys you change into \
         `~/.config/thegn/config.toml`.\n\n",
    );
    for line in &intro {
        out.push_str(&escape_wikilinks(line));
        out.push('\n');
    }

    for section in &sections {
        // Trim edge blanks from the fence body.
        let mut body: Vec<&str> = section.body.iter().map(String::as_str).collect();
        while body.first().is_some_and(|l| l.is_empty()) {
            body.remove(0);
        }
        while body.last().is_some_and(|l| l.is_empty()) {
            body.pop();
        }
        if body.is_empty() {
            continue;
        }
        out.push_str(&format!("\n## `{}`\n\n", section.title));
        for line in &section.prose {
            out.push_str(&escape_wikilinks(line));
            out.push('\n');
        }
        if !section.prose.is_empty() {
            out.push('\n');
        }
        out.push_str("```toml\n");
        for line in body {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("```\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::help::{Block, frontmatter, markdown};

    const EXAMPLE: &str = "\
# thegn config — copy and edit.
# All keys are optional.

# Where worktrees live.
worktrees_dir = \"~/.thegn/worktrees\"
picker = \"auto\"

# Visual tuning. The accent recolors
# every surface.
[theme]
# Named palette preset.
preset = \"prism\"

[ui]
language = \"auto\"

# Custom actions, e.g. [[actions]] entries.
[[actions]]
name = \"x\"
";

    #[test]
    fn generates_a_parseable_page() {
        let src = page(EXAMPLE);
        let (meta, body) = frontmatter::parse(&src).expect("generated page must parse");
        assert_eq!(meta.id, PAGE_ID);
        assert!(meta.generated);
        assert_eq!(meta.parent.as_deref(), Some("configuration"));
        let blocks = markdown::parse(body);
        assert!(matches!(&blocks[0], Block::Heading { level: 1, .. }));
        // No internal links may leak out of prose (they'd be dangling).
        assert!(
            markdown::links(&blocks)
                .iter()
                .all(|t| !matches!(t, crate::help::LinkTarget::Page(_)))
        );
    }

    #[test]
    fn intro_and_sections_are_extracted() {
        let src = page(EXAMPLE);
        assert!(src.contains("thegn config — copy and edit."), "{src}");
        assert!(src.contains("## `General`"));
        assert!(src.contains("## `[theme]`"));
        assert!(src.contains("## `[ui]`"));
        assert!(src.contains("## `[[actions]]`"));
    }

    #[test]
    fn table_prose_is_the_block_above_the_header() {
        let src = page(EXAMPLE);
        let theme = src.split("## `[theme]`").nth(1).unwrap();
        let theme = theme.split("```toml").next().unwrap();
        assert!(
            theme.contains("Visual tuning. The accent recolors"),
            "{theme}"
        );
        assert!(theme.contains("every surface."));
        // The key's own comment stays inside the fence, not the prose.
        assert!(!theme.contains("Named palette preset"));
    }

    #[test]
    fn key_comments_stay_in_the_fence() {
        let src = page(EXAMPLE);
        let theme_fence = src
            .split("## `[theme]`")
            .nth(1)
            .unwrap()
            .split("```toml\n")
            .nth(1)
            .unwrap()
            .split("```")
            .next()
            .unwrap();
        assert!(theme_fence.contains("[theme]"));
        assert!(theme_fence.contains("# Named palette preset."));
        assert!(theme_fence.contains("preset = \"prism\""));
    }

    #[test]
    fn general_section_holds_top_level_keys() {
        let src = page(EXAMPLE);
        let general = src
            .split("## `General`")
            .nth(1)
            .unwrap()
            .split("##")
            .next()
            .unwrap();
        assert!(general.contains("worktrees_dir"));
        assert!(general.contains("# Where worktrees live."));
        assert!(general.contains("picker = \"auto\""));
    }

    #[test]
    fn wikilink_mentions_in_prose_are_escaped() {
        let src = page(EXAMPLE);
        assert!(src.contains("`[[actions]]` entries"), "{src}");
    }

    #[test]
    fn escape_wikilinks_edges() {
        assert_eq!(escape_wikilinks("no links"), "no links");
        assert_eq!(escape_wikilinks("a [[b]] c [[d]]"), "a `[[b]]` c `[[d]]`");
        assert_eq!(escape_wikilinks("unclosed [[b"), "unclosed [[b");
    }

    #[test]
    fn empty_example_yields_a_valid_page() {
        let src = page("");
        let (meta, body) = frontmatter::parse(&src).unwrap();
        assert_eq!(meta.id, PAGE_ID);
        assert!(!markdown::parse(body).is_empty());
    }

    #[test]
    fn real_example_config_generates_cleanly() {
        // The actual shipped example — the strongest guard against drift.
        let example = include_str!("../../../../config/config.toml.example");
        let src = page(example);
        let (meta, body) = frontmatter::parse(&src).expect("real example must generate a page");
        assert_eq!(meta.id, PAGE_ID);
        let blocks = markdown::parse(body);
        assert!(
            blocks
                .iter()
                .filter(|b| matches!(b, Block::Code { .. }))
                .count()
                > 5,
            "expected many toml sections"
        );
        assert!(
            markdown::links(&blocks)
                .iter()
                .all(|t| !matches!(t, crate::help::LinkTarget::Page(_)))
        );
    }
}
