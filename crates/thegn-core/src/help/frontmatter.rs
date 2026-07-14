//! Frontmatter — the strict metadata header every help page starts with.
//!
//! Deliberately not YAML (no dependency, no surprises): a `---` fence, one
//! `key: value` per line, bracket lists for the two list keys, and *unknown
//! keys are errors* so a typo like `context:` can't silently unbind a page
//! from the UI. The host's ratchet test surfaces every parse error, which is
//! what makes strictness safe.
//!
//! ```text
//! ---
//! id: merge-queue
//! title: Merge queue
//! parent: workflows            # optional TOC nesting
//! order: 30                    # optional sort within the parent
//! contexts: [panel:merge]      # focus contexts this page documents
//! actions: [integrate]         # ACTION_SPECS ids this page documents
//! ---
//! ```

use std::fmt;

/// Parsed page metadata. `id` and `title` are required; everything else
/// defaults to empty/none.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PageMeta {
    /// Stable page id (kebab-case) — the `[[link]]` and TOC key.
    pub id: String,
    /// Human title shown in the TOC and search results.
    pub title: String,
    /// Parent page id for TOC nesting.
    pub parent: Option<String>,
    /// Sort key within the parent (unset sorts after all set, by title).
    pub order: Option<i64>,
    /// Focus-context keys (e.g. `zone:sidebar`, `panel:merge`) this page is
    /// the documentation target for.
    pub contexts: Vec<String>,
    /// Action ids this page documents (feeds the help ratchet).
    pub actions: Vec<String>,
    /// True for pages generated at runtime (keybindings, config reference);
    /// generated pages never count toward ratchet coverage.
    pub generated: bool,
}

/// Why a frontmatter header failed to parse. Line numbers are 1-based within
/// the page source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterError {
    /// The source does not start with a `---` line.
    MissingOpen,
    /// The opening `---` was never closed.
    Unterminated,
    /// A line inside the header is not `key: value`.
    BadLine { line: usize, text: String },
    /// A key this schema does not know (typos are errors by design).
    UnknownKey { line: usize, key: String },
    /// The same key given twice.
    DuplicateKey { line: usize, key: String },
    /// A known key with an unparseable value.
    BadValue {
        line: usize,
        key: String,
        reason: String,
    },
    /// A required key (`id`, `title`) was absent.
    MissingKey(&'static str),
}

impl fmt::Display for FrontmatterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingOpen => write!(f, "page must start with a `---` frontmatter line"),
            Self::Unterminated => write!(f, "frontmatter `---` fence is never closed"),
            Self::BadLine { line, text } => {
                write!(f, "line {line}: expected `key: value`, got {text:?}")
            }
            Self::UnknownKey { line, key } => write!(f, "line {line}: unknown key `{key}`"),
            Self::DuplicateKey { line, key } => write!(f, "line {line}: duplicate key `{key}`"),
            Self::BadValue { line, key, reason } => {
                write!(f, "line {line}: bad value for `{key}`: {reason}")
            }
            Self::MissingKey(key) => write!(f, "missing required key `{key}`"),
        }
    }
}

/// A page id: non-empty kebab-case (`[a-z0-9-]`, no leading `-`).
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A list item token: action ids and context keys (`panel:merge`,
/// `merge-drain`, `new_worktree`). Lowercase word chars plus `-_.:`.
fn valid_token(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | ':')
        })
}

fn parse_list(value: &str, key: &str, line: usize) -> Result<Vec<String>, FrontmatterError> {
    let bad = |reason: &str| FrontmatterError::BadValue {
        line,
        key: key.to_string(),
        reason: reason.to_string(),
    };
    let inner = value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .ok_or_else(|| bad("expected a bracket list like [a, b]"))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for item in inner.split(',') {
        let item = item.trim();
        // A trailing comma (prettier's multi-line list style) leaves one
        // empty segment; skip it.
        if item.is_empty() {
            continue;
        }
        if !valid_token(item) {
            return Err(bad(&format!("bad list item {item:?}")));
        }
        out.push(item.to_string());
    }
    Ok(out)
}

/// Parse a page source into its metadata and the body slice after the
/// closing `---`. Blank lines and full-line `#` comments inside the header
/// are allowed; anything else must be a known `key: value`.
pub fn parse(src: &str) -> Result<(PageMeta, &str), FrontmatterError> {
    let mut lines = src.split_inclusive('\n');
    let first = lines.next().ok_or(FrontmatterError::MissingOpen)?;
    if first.trim_end() != "---" {
        return Err(FrontmatterError::MissingOpen);
    }
    let mut off = first.len();
    let mut lineno = 1usize;
    let mut meta = PageMeta::default();
    let mut seen: Vec<String> = Vec::new();
    let mut closed = false;

    while let Some(line) = lines.next() {
        off += line.len();
        lineno += 1;
        let t = line.trim_end();
        if t == "---" {
            closed = true;
            break;
        }
        let s = t.trim();
        if s.is_empty() || s.starts_with('#') {
            continue;
        }
        let Some((key, value)) = s.split_once(':') else {
            return Err(FrontmatterError::BadLine {
                line: lineno,
                text: s.to_string(),
            });
        };
        let (key, mut value) = (key.trim(), value.trim().to_string());
        // The list keys may fold across lines (prettier's multi-line bracket
        // style, `actions:\n  [\n    a,\n  ]`): keep consuming to the `]`.
        if matches!(key, "contexts" | "actions") {
            while !value.contains(']') {
                let Some(cont) = lines.next() else {
                    return Err(FrontmatterError::BadValue {
                        line: lineno,
                        key: key.to_string(),
                        reason: "unterminated bracket list".to_string(),
                    });
                };
                off += cont.len();
                lineno += 1;
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(cont.trim());
            }
        }
        let value = value.as_str();
        if seen.iter().any(|k| k == key) {
            return Err(FrontmatterError::DuplicateKey {
                line: lineno,
                key: key.to_string(),
            });
        }
        seen.push(key.to_string());
        let bad = |reason: &str| FrontmatterError::BadValue {
            line: lineno,
            key: key.to_string(),
            reason: reason.to_string(),
        };
        match key {
            "id" => {
                if !valid_id(value) {
                    return Err(bad("expected a kebab-case id"));
                }
                meta.id = value.to_string();
            }
            "title" => {
                if value.is_empty() {
                    return Err(bad("title must be non-empty"));
                }
                meta.title = value.to_string();
            }
            "parent" => {
                if !valid_id(value) {
                    return Err(bad("expected a kebab-case page id"));
                }
                meta.parent = Some(value.to_string());
            }
            "order" => {
                meta.order = Some(
                    value
                        .parse::<i64>()
                        .map_err(|_| bad("expected an integer"))?,
                );
            }
            "contexts" => meta.contexts = parse_list(value, key, lineno)?,
            "actions" => meta.actions = parse_list(value, key, lineno)?,
            "generated" => {
                meta.generated = match value {
                    "true" => true,
                    "false" => false,
                    _ => return Err(bad("expected true or false")),
                };
            }
            _ => {
                return Err(FrontmatterError::UnknownKey {
                    line: lineno,
                    key: key.to_string(),
                });
            }
        }
    }

    if !closed {
        return Err(FrontmatterError::Unterminated);
    }
    if meta.id.is_empty() {
        return Err(FrontmatterError::MissingKey("id"));
    }
    if meta.title.is_empty() {
        return Err(FrontmatterError::MissingKey("title"));
    }
    Ok((meta, &src[off..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = "---\n\
                        id: merge-queue\n\
                        title: Merge queue\n\
                        parent: workflows\n\
                        order: 30\n\
                        # a comment, and a blank line below\n\
                        \n\
                        contexts: [panel:merge, zone:sidebar]\n\
                        actions: [integrate, merge-drain]\n\
                        generated: false\n\
                        ---\n\
                        # Body\n";

    #[test]
    fn parses_a_full_header() {
        let (meta, body) = parse(GOOD).unwrap();
        assert_eq!(meta.id, "merge-queue");
        assert_eq!(meta.title, "Merge queue");
        assert_eq!(meta.parent.as_deref(), Some("workflows"));
        assert_eq!(meta.order, Some(30));
        assert_eq!(meta.contexts, vec!["panel:merge", "zone:sidebar"]);
        assert_eq!(meta.actions, vec!["integrate", "merge-drain"]);
        assert!(!meta.generated);
        assert_eq!(body, "# Body\n");
    }

    #[test]
    fn minimal_header_defaults() {
        let (meta, body) = parse("---\nid: x\ntitle: X\n---\nbody").unwrap();
        assert_eq!(meta.parent, None);
        assert_eq!(meta.order, None);
        assert!(meta.contexts.is_empty() && meta.actions.is_empty());
        assert!(!meta.generated);
        assert_eq!(body, "body");
    }

    #[test]
    fn prettier_multiline_lists_parse() {
        // prettier folds long frontmatter lists across lines with a trailing
        // comma; the body offset must stay exact through the continuation.
        let src = "---\n\
                   id: x\n\
                   title: X\n\
                   actions:\n\
                   \x20 [\n\
                   \x20   palette,\n\
                   \x20   switch-font,\n\
                   \x20 ]\n\
                   contexts: [zone:sidebar]\n\
                   ---\n\
                   body\n";
        let (meta, body) = parse(src).unwrap();
        assert_eq!(meta.actions, vec!["palette", "switch-font"]);
        assert_eq!(meta.contexts, vec!["zone:sidebar"]);
        assert_eq!(body, "body\n");
        // Unterminated fold is an error, not a hang.
        let err = parse("---\nid: x\ntitle: X\nactions:\n  [\n    a,\n").unwrap_err();
        assert!(matches!(err, FrontmatterError::BadValue { .. }), "{err:?}");
    }

    #[test]
    fn empty_lists_and_generated_true() {
        let (meta, _) =
            parse("---\nid: x\ntitle: X\ncontexts: []\ngenerated: true\n---\n").unwrap();
        assert!(meta.contexts.is_empty());
        assert!(meta.generated);
    }

    #[test]
    fn missing_open_fence() {
        assert_eq!(parse("id: x\n---\n"), Err(FrontmatterError::MissingOpen));
        assert_eq!(parse(""), Err(FrontmatterError::MissingOpen));
    }

    #[test]
    fn unterminated_fence() {
        assert_eq!(
            parse("---\nid: x\ntitle: X\n"),
            Err(FrontmatterError::Unterminated)
        );
    }

    #[test]
    fn unknown_key_is_an_error() {
        // The whole point: `context:` (singular typo) must not parse.
        let err = parse("---\nid: x\ntitle: X\ncontext: [zone:sidebar]\n---\n").unwrap_err();
        assert!(matches!(err, FrontmatterError::UnknownKey { key, .. } if key == "context"));
    }

    #[test]
    fn duplicate_key_is_an_error() {
        let err = parse("---\nid: x\nid: y\ntitle: X\n---\n").unwrap_err();
        assert!(matches!(err, FrontmatterError::DuplicateKey { key, line: 3 } if key == "id"));
    }

    #[test]
    fn bad_line_without_colon() {
        let err = parse("---\nid: x\njust words\ntitle: X\n---\n").unwrap_err();
        assert!(matches!(err, FrontmatterError::BadLine { line: 3, .. }));
    }

    #[test]
    fn bad_values() {
        for (header, key) in [
            ("id: Not Kebab", "id"),
            ("id: -leading", "id"),
            ("title:", "title"),
            ("parent: Bad Parent", "parent"),
            ("order: soon", "order"),
            ("contexts: zone:sidebar", "contexts"),
            ("actions: [has space]", "actions"),
            ("generated: yes", "generated"),
        ] {
            let src = format!("---\nid: x\ntitle: X\n{header}\n---\n");
            // `id`/`title` bad-value cases re-declare the key, so build those
            // sources without the good copy.
            let src = if key == "id" || key == "title" {
                format!("---\n{header}\n---\n")
            } else {
                src
            };
            let err = parse(&src).unwrap_err();
            assert!(
                matches!(&err, FrontmatterError::BadValue { key: k, .. } if k == key),
                "{header}: {err:?}"
            );
        }
    }

    #[test]
    fn missing_required_keys() {
        assert_eq!(
            parse("---\ntitle: X\n---\n"),
            Err(FrontmatterError::MissingKey("id"))
        );
        assert_eq!(
            parse("---\nid: x\n---\n"),
            Err(FrontmatterError::MissingKey("title"))
        );
    }

    #[test]
    fn errors_display_readably() {
        // Display is user-facing (ratchet-test failure messages) — keep every
        // variant rendering something meaningful.
        let cases: Vec<(FrontmatterError, &str)> = vec![
            (FrontmatterError::MissingOpen, "must start"),
            (FrontmatterError::Unterminated, "never closed"),
            (
                FrontmatterError::BadLine {
                    line: 2,
                    text: "x".into(),
                },
                "line 2",
            ),
            (
                FrontmatterError::UnknownKey {
                    line: 3,
                    key: "ctx".into(),
                },
                "`ctx`",
            ),
            (
                FrontmatterError::DuplicateKey {
                    line: 4,
                    key: "id".into(),
                },
                "duplicate",
            ),
            (
                FrontmatterError::BadValue {
                    line: 5,
                    key: "order".into(),
                    reason: "int".into(),
                },
                "order",
            ),
            (FrontmatterError::MissingKey("title"), "`title`"),
        ];
        for (err, needle) in cases {
            let s = err.to_string();
            assert!(s.contains(needle), "{s:?} should contain {needle:?}");
        }
    }

    #[test]
    fn valid_id_edges() {
        assert!(valid_id("a"));
        assert!(valid_id("merge-queue-2"));
        assert!(!valid_id(""));
        assert!(!valid_id("-x"));
        assert!(!valid_id("Caps"));
        assert!(!valid_id("under_score"));
    }
}
