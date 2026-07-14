//! A deliberately small markdown subset for help pages — no dependency, no
//! HTML, no surprises. Anything the grammar doesn't recognize degrades to
//! literal text; the parser never fails and never panics.
//!
//! Blocks: `#`/`##`/`###` headings, fenced code, `-`/`*`/`1.` lists (one
//! nesting level via 2-space indent, indented continuation lines flow into
//! the item), `>` quotes, `---` rules, paragraphs.
//! Inlines: `**bold**`, `*italic*` / `_italic_` (underscores only at word
//! boundaries, so `snake_case` identifiers stay literal — prettier rewrites
//! emphasis to underscores, so both spellings must parse), `` `code` ``,
//! `[label](url)` external links, and `[[page-id]]` / `[[page-id|label]]`
//! internal links.

/// Where a link points: another help page (validated by the registry) or an
/// external URL (surfaced, not followed, by the TUI).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Page(String),
    Url(String),
}

/// A styled run within a line of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text(String),
    Bold(String),
    Italic(String),
    Code(String),
    Link { target: LinkTarget, label: String },
}

/// One list bullet. `depth` is 0 for top-level items, 1 for the single
/// supported nesting level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    pub depth: u8,
    pub ordered: bool,
    pub spans: Vec<Inline>,
}

/// A block-level element of a help page body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    /// `level` is clamped to 1..=3.
    Heading {
        level: u8,
        spans: Vec<Inline>,
    },
    Para(Vec<Inline>),
    Code {
        lang: String,
        text: String,
    },
    List(Vec<ListItem>),
    Quote(Vec<Inline>),
    Rule,
}

/// `- item` / `* item` / `12. item`, with a 2-space indent marking depth 1.
fn list_item(line: &str) -> Option<(u8, bool, &str)> {
    let indent = line.len() - line.trim_start().len();
    let depth = u8::from(indent >= 2);
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
        return Some((depth, false, rest.trim_start()));
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits > 0
        && let Some(rest) = t[digits..].strip_prefix(". ")
    {
        return Some((depth, true, rest.trim_start()));
    }
    None
}

fn is_rule(t: &str) -> bool {
    t.len() >= 3 && t.chars().all(|c| c == '-')
}

/// Does this line open a non-paragraph block? (Used to end a paragraph run.)
fn starts_block(t: &str) -> bool {
    t.starts_with("```")
        || t.starts_with('>')
        || is_rule(t)
        || list_item(t).is_some()
        || heading_level(t).is_some()
}

/// `(level, rest)` for `# `-style headings; `####`+ clamps to 3.
fn heading_level(t: &str) -> Option<(u8, &str)> {
    let hashes = t.chars().take_while(|&c| c == '#').count();
    if hashes == 0 {
        return None;
    }
    let rest = &t[hashes..];
    let rest = rest.strip_prefix(' ')?;
    Some((hashes.min(3) as u8, rest.trim_start()))
}

/// Parse a page body into blocks. Total: no input fails.
pub fn parse(body: &str) -> Vec<Block> {
    let lines: Vec<&str> = body.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end();
        let t = line.trim_start();
        if t.is_empty() {
            i += 1;
            continue;
        }
        // Fenced code — everything verbatim until the closing fence (or EOF).
        if let Some(rest) = t.strip_prefix("```") {
            let lang = rest.trim().to_string();
            let mut text = String::new();
            i += 1;
            let mut first = true;
            while i < lines.len() && lines[i].trim() != "```" {
                if !first {
                    text.push('\n');
                }
                text.push_str(lines[i].trim_end());
                first = false;
                i += 1;
            }
            if i < lines.len() {
                i += 1; // consume the closing fence
            }
            out.push(Block::Code { lang, text });
            continue;
        }
        if let Some((level, rest)) = heading_level(t) {
            out.push(Block::Heading {
                level,
                spans: inlines(rest),
            });
            i += 1;
            continue;
        }
        if is_rule(t) {
            out.push(Block::Rule);
            i += 1;
            continue;
        }
        // Quote — contiguous `>` lines flow into one block.
        if t.starts_with('>') {
            let mut parts: Vec<&str> = Vec::new();
            while i < lines.len() {
                let qt = lines[i].trim_start();
                let Some(inner) = qt.strip_prefix('>') else {
                    break;
                };
                let inner = inner.strip_prefix(' ').unwrap_or(inner).trim_end();
                if !inner.is_empty() {
                    parts.push(inner);
                }
                i += 1;
            }
            out.push(Block::Quote(inlines(&parts.join(" "))));
            continue;
        }
        // List — contiguous bullet lines form one block; an indented
        // non-bullet line flows into the item above it (the hanging-indent
        // wrap style prettier produces).
        if list_item(line).is_some() {
            let mut items: Vec<(u8, bool, String)> = Vec::new();
            while i < lines.len() {
                let raw = lines[i].trim_end();
                if let Some((depth, ordered, rest)) = list_item(raw) {
                    items.push((depth, ordered, rest.to_string()));
                    i += 1;
                    continue;
                }
                if raw.starts_with(' ')
                    && !raw.trim().is_empty()
                    && let Some(last) = items.last_mut()
                {
                    last.2.push(' ');
                    last.2.push_str(raw.trim());
                    i += 1;
                    continue;
                }
                break;
            }
            out.push(Block::List(
                items
                    .into_iter()
                    .map(|(depth, ordered, text)| ListItem {
                        depth,
                        ordered,
                        spans: inlines(&text),
                    })
                    .collect(),
            ));
            continue;
        }
        // Paragraph — run until a blank line or the start of another block.
        let mut parts = vec![t];
        i += 1;
        while i < lines.len() {
            let next = lines[i].trim_end().trim_start();
            if next.is_empty() || starts_block(next) {
                break;
            }
            parts.push(next);
            i += 1;
        }
        out.push(Block::Para(inlines(&parts.join(" "))));
    }
    out
}

/// Parse inline markup within one logical line. Unclosed or empty markers
/// degrade to literal text.
pub fn inlines(text: &str) -> Vec<Inline> {
    fn flush(buf: &mut String, out: &mut Vec<Inline>) {
        if !buf.is_empty() {
            out.push(Inline::Text(std::mem::take(buf)));
        }
    }
    fn emphasis_ok(inner: &str) -> bool {
        !inner.is_empty()
            && !inner.starts_with(char::is_whitespace)
            && !inner.ends_with(char::is_whitespace)
    }

    let mut out = Vec::new();
    let mut buf = String::new();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        // [[page-id]] / [[page-id|label]]
        if let Some(after) = rest.strip_prefix("[[")
            && let Some(end) = after.find("]]")
        {
            let inner = &after[..end];
            let (id, label) = match inner.split_once('|') {
                Some((a, b)) => (a.trim(), b.trim()),
                None => (inner.trim(), inner.trim()),
            };
            if super::frontmatter::valid_id(id) && !label.is_empty() {
                flush(&mut buf, &mut out);
                out.push(Inline::Link {
                    target: LinkTarget::Page(id.to_string()),
                    label: label.to_string(),
                });
                i += 2 + end + 2;
                continue;
            }
        }
        // **bold**
        if let Some(after) = rest.strip_prefix("**")
            && let Some(end) = after.find("**")
        {
            let inner = &after[..end];
            if emphasis_ok(inner) {
                flush(&mut buf, &mut out);
                out.push(Inline::Bold(inner.to_string()));
                i += 2 + end + 2;
                continue;
            }
        }
        // `code`
        if let Some(after) = rest.strip_prefix('`')
            && let Some(end) = after.find('`')
        {
            let inner = &after[..end];
            if !inner.is_empty() {
                flush(&mut buf, &mut out);
                out.push(Inline::Code(inner.to_string()));
                i += 1 + end + 1;
                continue;
            }
        }
        // [label](url)
        if rest.starts_with('[')
            && !rest.starts_with("[[")
            && let Some(close) = rest.find("](")
        {
            let label = &rest[1..close];
            if let Some(paren) = rest[close + 2..].find(')') {
                let url = &rest[close + 2..close + 2 + paren];
                if !label.is_empty() && !url.is_empty() && !label.contains('[') {
                    flush(&mut buf, &mut out);
                    out.push(Inline::Link {
                        target: LinkTarget::Url(url.to_string()),
                        label: label.to_string(),
                    });
                    i += close + 2 + paren + 1;
                    continue;
                }
            }
        }
        // *italic* (checked after ** so a bold opener never half-matches)
        if rest.starts_with('*')
            && !rest.starts_with("**")
            && let Some(end) = rest[1..].find('*')
        {
            let inner = &rest[1..1 + end];
            if emphasis_ok(inner) {
                flush(&mut buf, &mut out);
                out.push(Inline::Italic(inner.to_string()));
                i += 1 + end + 1;
                continue;
            }
        }
        // _italic_ — only at word boundaries, so `worktrees_dir` stays text:
        // the opener can't follow an alphanumeric, the closer can't precede one.
        if rest.starts_with('_')
            && text[..i]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric())
            && let Some(end) = rest[1..].find('_')
        {
            let inner = &rest[1..1 + end];
            let after = rest[1 + end + 1..].chars().next();
            if emphasis_ok(inner) && after.is_none_or(|c| !c.is_alphanumeric()) {
                flush(&mut buf, &mut out);
                out.push(Inline::Italic(inner.to_string()));
                i += 1 + end + 1;
                continue;
            }
        }
        let ch = rest.chars().next().expect("i is on a char boundary");
        buf.push(ch);
        i += ch.len_utf8();
    }
    flush(&mut buf, &mut out);
    out
}

/// The unstyled text of a span run — search corpus, TOC labels, snippets.
pub fn plain(spans: &[Inline]) -> String {
    spans
        .iter()
        .map(|s| match s {
            Inline::Text(t) | Inline::Bold(t) | Inline::Italic(t) | Inline::Code(t) => t.as_str(),
            Inline::Link { label, .. } => label.as_str(),
        })
        .collect()
}

/// Every link target in a parsed body, in document order (registry
/// validation walks this).
pub fn links(blocks: &[Block]) -> Vec<&LinkTarget> {
    fn from_spans<'a>(spans: &'a [Inline], out: &mut Vec<&'a LinkTarget>) {
        for s in spans {
            if let Inline::Link { target, .. } = s {
                out.push(target);
            }
        }
    }
    let mut out = Vec::new();
    for b in blocks {
        match b {
            Block::Heading { spans, .. } | Block::Para(spans) | Block::Quote(spans) => {
                from_spans(spans, &mut out);
            }
            Block::List(items) => {
                for it in items {
                    from_spans(&it.spans, &mut out);
                }
            }
            Block::Code { .. } | Block::Rule => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Inline {
        Inline::Text(s.to_string())
    }

    #[test]
    fn headings_clamp_and_parse_inlines() {
        let blocks = parse("# One\n## Two\n### Three\n#### Four\n# **Bold** title\n");
        assert_eq!(blocks.len(), 5);
        for (i, want) in [1u8, 2, 3, 3].iter().enumerate() {
            let Block::Heading { level, .. } = &blocks[i] else {
                panic!("{blocks:?}")
            };
            assert_eq!(level, want);
        }
        let Block::Heading { spans, .. } = &blocks[4] else {
            panic!()
        };
        assert_eq!(spans[0], Inline::Bold("Bold".into()));
    }

    #[test]
    fn hashtag_without_space_is_a_paragraph() {
        let blocks = parse("#nospace\n");
        assert_eq!(blocks, vec![Block::Para(vec![text("#nospace")])]);
    }

    #[test]
    fn paragraphs_join_lines_and_split_on_blanks() {
        let blocks = parse("one\ntwo\n\nthree\n");
        assert_eq!(
            blocks,
            vec![
                Block::Para(vec![text("one two")]),
                Block::Para(vec![text("three")])
            ]
        );
    }

    #[test]
    fn paragraph_ends_where_a_block_starts() {
        let blocks = parse("prose\n- item\n");
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[1], Block::List(items) if items.len() == 1));
    }

    #[test]
    fn fenced_code_is_verbatim() {
        let blocks = parse("```toml\nkey = 1   \n\n  indented\n```\nafter\n");
        assert_eq!(
            blocks[0],
            Block::Code {
                lang: "toml".into(),
                text: "key = 1\n\n  indented".into()
            }
        );
        assert_eq!(blocks[1], Block::Para(vec![text("after")]));
    }

    #[test]
    fn unclosed_fence_takes_the_rest() {
        let blocks = parse("```\nhello\nworld");
        assert_eq!(
            blocks,
            vec![Block::Code {
                lang: String::new(),
                text: "hello\nworld".into()
            }]
        );
    }

    #[test]
    fn empty_fence() {
        let blocks = parse("```\n```\n");
        assert_eq!(
            blocks,
            vec![Block::Code {
                lang: String::new(),
                text: String::new()
            }]
        );
    }

    #[test]
    fn lists_with_nesting_and_ordering() {
        let blocks = parse("- a\n- b\n  - b1\n1. c\n");
        let Block::List(items) = &blocks[0] else {
            panic!("{blocks:?}")
        };
        assert_eq!(items.len(), 4);
        assert_eq!((items[0].depth, items[0].ordered), (0, false));
        assert_eq!((items[2].depth, items[2].ordered), (1, false));
        assert_eq!((items[3].depth, items[3].ordered), (0, true));
        assert_eq!(plain(&items[2].spans), "b1");
    }

    #[test]
    fn quotes_flow_contiguous_lines() {
        let blocks = parse("> tip: use\n> the queue\nplain\n");
        assert_eq!(blocks[0], Block::Quote(vec![text("tip: use the queue")]));
        assert_eq!(blocks[1], Block::Para(vec![text("plain")]));
    }

    #[test]
    fn rule_vs_list_vs_frontmatterish() {
        let blocks = parse("---\n--\n- x\n");
        assert_eq!(blocks[0], Block::Rule);
        // "--" is too short for a rule → paragraph text.
        assert_eq!(blocks[1], Block::Para(vec![text("--")]));
        assert!(matches!(&blocks[2], Block::List(_)));
    }

    #[test]
    fn inline_bold_italic_code() {
        assert_eq!(
            inlines("a **b** *c* `d` e"),
            vec![
                text("a "),
                Inline::Bold("b".into()),
                text(" "),
                Inline::Italic("c".into()),
                text(" "),
                Inline::Code("d".into()),
                text(" e"),
            ]
        );
    }

    #[test]
    fn inline_links() {
        assert_eq!(
            inlines("see [[merge-queue]] and [[sidebar|the tree]] or [docs](https://x.dev)"),
            vec![
                text("see "),
                Inline::Link {
                    target: LinkTarget::Page("merge-queue".into()),
                    label: "merge-queue".into()
                },
                text(" and "),
                Inline::Link {
                    target: LinkTarget::Page("sidebar".into()),
                    label: "the tree".into()
                },
                text(" or "),
                Inline::Link {
                    target: LinkTarget::Url("https://x.dev".into()),
                    label: "docs".into()
                },
            ]
        );
    }

    #[test]
    fn unclosed_markers_degrade_to_text() {
        assert_eq!(inlines("**open"), vec![text("**open")]);
        assert_eq!(inlines("`tick"), vec![text("`tick")]);
        assert_eq!(inlines("[[bad"), vec![text("[[bad")]);
        assert_eq!(inlines("[label](open"), vec![text("[label](open")]);
        assert_eq!(inlines("2 * 3 * 4"), vec![text("2 * 3 * 4")]);
        // Empty emphasis is literal.
        assert_eq!(inlines("****"), vec![text("****")]);
        assert_eq!(inlines("``"), vec![text("``")]);
    }

    #[test]
    fn wikilink_with_bad_id_is_literal() {
        assert_eq!(inlines("[[Not An Id]]"), vec![text("[[Not An Id]]")]);
        assert_eq!(inlines("[[x|]]"), vec![text("[[x|]]")]);
    }

    #[test]
    fn backticked_wikilink_stays_code() {
        // config_ref leans on this: `[[actions]]` in prose must not become a link.
        assert_eq!(
            inlines("`[[actions]]`"),
            vec![Inline::Code("[[actions]]".into())]
        );
    }

    #[test]
    fn underscore_italics_only_at_word_boundaries() {
        // prettier rewrites emphasis to underscores; both spellings parse.
        assert_eq!(
            inlines("a _b c_ d"),
            vec![text("a "), Inline::Italic("b c".into()), text(" d")]
        );
        assert_eq!(inlines("_lead_"), vec![Inline::Italic("lead".into())]);
        // Word-internal underscores stay literal.
        assert_eq!(
            inlines("worktrees_dir stays"),
            vec![text("worktrees_dir stays")]
        );
        assert_eq!(inlines("_foo_bar"), vec![text("_foo_bar")]);
        assert_eq!(inlines("x_y_z"), vec![text("x_y_z")]);
    }

    #[test]
    fn list_continuation_lines_flow_into_the_item() {
        let blocks = parse("- first item that\n  wraps onward\n- second\n");
        let Block::List(items) = &blocks[0] else {
            panic!("{blocks:?}")
        };
        assert_eq!(items.len(), 2);
        assert_eq!(plain(&items[0].spans), "first item that wraps onward");
        assert_eq!(plain(&items[1].spans), "second");
        // Nested items are still items, not continuations.
        let blocks = parse("- top\n  - nested\n    nested wrap\n");
        let Block::List(items) = &blocks[0] else {
            panic!("{blocks:?}")
        };
        assert_eq!(items.len(), 2);
        assert_eq!(plain(&items[1].spans), "nested nested wrap");
    }

    #[test]
    fn snake_case_is_untouched() {
        assert_eq!(
            inlines("worktrees_dir and base_branch"),
            vec![text("worktrees_dir and base_branch")]
        );
    }

    #[test]
    fn multibyte_text_survives() {
        let spans = inlines("naïve — **強調** done");
        assert_eq!(
            spans,
            vec![text("naïve — "), Inline::Bold("強調".into()), text(" done")]
        );
        assert_eq!(plain(&spans), "naïve — 強調 done");
    }

    #[test]
    fn links_walks_every_container() {
        let blocks = parse(
            "# see [[a]]\npara [[b]]\n- item [[c]]\n> quote [[d]]\n```\n[[not-a-link]]\n```\n",
        );
        let ids: Vec<String> = links(&blocks)
            .into_iter()
            .map(|t| match t {
                LinkTarget::Page(p) => p.clone(),
                LinkTarget::Url(u) => u.clone(),
            })
            .collect();
        assert_eq!(ids, vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn empty_body_parses_to_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("\n\n\n").is_empty());
    }
}
