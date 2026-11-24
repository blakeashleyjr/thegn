//! The shared argv-template tokenizer (resolutions B20): one pure grammar for
//! every config-defined argv (sandbox backends, providers, connectors, plugin
//! bridges, agent presets) instead of five.
//!
//! Skeleton landed by WS00-P1-07; owner after WS00: WS02-P4-02, which adds the
//! structural constructs (`each`, `when`, `splat`, `join`, filters), list
//! tokens, per-verb token sets and `StaticTokenSet`/`TokenDef`. Consumers:
//! WS07, WS08-P3-02, WS09-P1-02, WS10-P8-01 and WS11-P1-03, each with its own
//! token set.
//!
//! The grammar of one argv element:
//! - `{a.b.c}` is a token path; each segment matches `[a-z][a-z0-9_]*`;
//! - `{{` is a literal `{` and `}}` a literal `}`;
//! - everything else is literal UTF-8 text, including spaces, quotes,
//!   backslashes, dollar signs and NUL bytes.
//!
//! This is not a shell lexer: quote and escape characters are copied as argv
//! content and have no grouping or evaluation semantics. The caller supplies the
//! argv element boundaries. Process-launch APIs may reject a value such as NUL
//! later; parsing and rendering never execute or normalise it.
//!
//! **One element in, exactly one string out.** A token's value is inserted
//! verbatim: it is never split on whitespace and never re-parsed, so a value
//! containing spaces or braces cannot become extra argv elements or new tokens.
//! Parsing and rendering are each capped at [`MAX_TEMPLATE_ELEMENT_BYTES`] per
//! element, so configured input and token expansion have an explicit bound.

use std::fmt;

/// Largest accepted template or rendered argv element, in UTF-8 bytes. This
/// matches Linux's per-argument limit (`MAX_ARG_STRLEN`, 32 pages), so agent
/// prompts passed as one element (up to ~100 KiB today) still fit.
pub const MAX_TEMPLATE_ELEMENT_BYTES: usize = 128 * 1024;

/// One parsed piece of an argv element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Literal text (escapes already resolved).
    Lit(String),
    /// A token to substitute.
    Tok(TokenPath),
}

/// A token path such as `limits.cpus` (segments `[a-z][a-z0-9_]*`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenPath(Vec<String>);

impl TokenPath {
    /// The path's segments, in order.
    pub fn segments(&self) -> &[String] {
        &self.0
    }
}

impl fmt::Display for TokenPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join("."))
    }
}

/// One argv element: literal text and tokens, rendered to exactly one string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Element(Vec<Piece>);

impl Element {
    /// The element's pieces, in order. Adjacent literal text is one piece.
    pub fn pieces(&self) -> &[Piece] {
        &self.0
    }
}

/// Why a template element was rejected. `at` is a byte offset into the element
/// source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    /// A `{` without its `}` (or with another `{` inside), or a lone `}`.
    Unbalanced {
        /// Byte offset of the offending brace.
        at: usize,
    },
    /// `{}`.
    EmptyToken {
        /// Byte offset of the opening brace.
        at: usize,
    },
    /// A path segment that is not `[a-z][a-z0-9_]*`.
    BadSegment {
        /// Byte offset of the token's opening brace.
        at: usize,
        /// The offending segment.
        seg: String,
    },
    /// The template input or the rendered element exceeds the per-element cap.
    TooLarge {
        /// The maximum accepted byte length.
        limit: usize,
    },
    /// A token the token set does not allow, or the source cannot supply.
    UnknownToken {
        /// The dotted token path.
        path: String,
    },
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::Unbalanced { at } => write!(f, "unbalanced brace at byte {at}"),
            TemplateError::EmptyToken { at } => write!(f, "empty token at byte {at}"),
            TemplateError::BadSegment { at, seg } => {
                write!(f, "bad token segment {seg:?} at byte {at}")
            }
            TemplateError::TooLarge { limit } => {
                write!(f, "argv template element exceeds {limit} bytes")
            }
            TemplateError::UnknownToken { path } => write!(f, "unknown token {{{path}}}"),
        }
    }
}

impl std::error::Error for TemplateError {}

/// The tokens a template may reference (per consumer, per verb).
pub trait TokenSet {
    /// Whether `path` is a known token.
    fn allows(&self, path: &TokenPath) -> bool;
}

/// Where token values come from at render time.
pub trait TokenSource {
    /// The scalar value of `path`, or `None` when it has none.
    fn scalar(&self, path: &TokenPath) -> Option<String>;
}

fn valid_segment(seg: &str) -> bool {
    let mut bytes = seg.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// Parse one argv element. `{{` → `{`, `}}` → `}`; `{a.b}` is a token.
pub fn parse_element(s: &str) -> Result<Element, TemplateError> {
    if s.len() > MAX_TEMPLATE_ELEMENT_BYTES {
        return Err(TemplateError::TooLarge {
            limit: MAX_TEMPLATE_ELEMENT_BYTES,
        });
    }
    let bytes = s.as_bytes();
    let mut pieces = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'{' if bytes.get(i + 1) == Some(&b'{') => {
                lit.push('{');
                i += 2;
            }
            b'}' if bytes.get(i + 1) == Some(&b'}') => {
                lit.push('}');
                i += 2;
            }
            b'}' => return Err(TemplateError::Unbalanced { at: i }),
            b'{' => {
                let close = bytes[i + 1..]
                    .iter()
                    .position(|&b| b == b'{' || b == b'}')
                    .map(|offset| i + 1 + offset);
                let close = match close {
                    Some(close) if bytes[close] == b'}' => close,
                    _ => return Err(TemplateError::Unbalanced { at: i }),
                };
                let inner = &s[i + 1..close];
                if inner.is_empty() {
                    return Err(TemplateError::EmptyToken { at: i });
                }
                let mut segments = Vec::new();
                for seg in inner.split('.') {
                    if !valid_segment(seg) {
                        return Err(TemplateError::BadSegment {
                            at: i,
                            seg: seg.to_string(),
                        });
                    }
                    segments.push(seg.to_string());
                }
                if !lit.is_empty() {
                    pieces.push(Piece::Lit(std::mem::take(&mut lit)));
                }
                pieces.push(Piece::Tok(TokenPath(segments)));
                i = close + 1;
            }
            _ => {
                // Copy the run of literal text up to the next brace. Braces are
                // ASCII, so both ends are char boundaries.
                let end = bytes[i..]
                    .iter()
                    .position(|&b| b == b'{' || b == b'}')
                    .map_or(bytes.len(), |offset| i + offset);
                lit.push_str(&s[i..end]);
                i = end;
            }
        }
    }
    if !lit.is_empty() {
        pieces.push(Piece::Lit(lit));
    }
    Ok(Element(pieces))
}

/// Check every token of `e` against `set`; the first unknown one is the error.
pub fn validate(e: &Element, set: &dyn TokenSet) -> Result<(), TemplateError> {
    for piece in &e.0 {
        if let Piece::Tok(path) = piece
            && !set.allows(path)
        {
            return Err(TemplateError::UnknownToken {
                path: path.to_string(),
            });
        }
    }
    Ok(())
}

/// Render `e` to exactly one string: literals and scalar values concatenated
/// verbatim (never split, never re-parsed). A token without a value is
/// [`TemplateError::UnknownToken`]; a result above [`MAX_TEMPLATE_ELEMENT_BYTES`]
/// is [`TemplateError::TooLarge`].
pub fn render_element(e: &Element, src: &dyn TokenSource) -> Result<String, TemplateError> {
    let mut out = String::new();
    for piece in &e.0 {
        match piece {
            Piece::Lit(text) => push_bounded(&mut out, text)?,
            Piece::Tok(path) => match src.scalar(path) {
                Some(value) => push_bounded(&mut out, &value)?,
                None => {
                    return Err(TemplateError::UnknownToken {
                        path: path.to_string(),
                    });
                }
            },
        }
    }
    Ok(out)
}

/// Append `text` to `out`, refusing to cross the per-element cap.
fn push_bounded(out: &mut String, text: &str) -> Result<(), TemplateError> {
    if text.len() > MAX_TEMPLATE_ELEMENT_BYTES - out.len() {
        return Err(TemplateError::TooLarge {
            limit: MAX_TEMPLATE_ELEMENT_BYTES,
        });
    }
    out.push_str(text);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A token set and source over a fixed map.
    struct Map(BTreeMap<&'static str, String>);

    impl Map {
        fn new(pairs: &[(&'static str, &str)]) -> Self {
            Map(pairs.iter().map(|(k, v)| (*k, v.to_string())).collect())
        }
    }

    impl TokenSet for Map {
        fn allows(&self, path: &TokenPath) -> bool {
            self.0.contains_key(path.to_string().as_str())
        }
    }

    impl TokenSource for Map {
        fn scalar(&self, path: &TokenPath) -> Option<String> {
            self.0.get(path.to_string().as_str()).cloned()
        }
    }

    fn render(template: &str, map: &Map) -> Result<String, TemplateError> {
        let element = parse_element(template)?;
        validate(&element, map)?;
        render_element(&element, map)
    }

    #[test]
    fn escapes_double_braces() {
        let element = parse_element("a{{b}}c").unwrap();
        assert_eq!(element.pieces(), &[Piece::Lit("a{b}c".into())]);
        assert_eq!(
            render("{{{name}}}", &Map::new(&[("name", "x")])).unwrap(),
            "{x}"
        );
        assert!(parse_element("").unwrap().pieces().is_empty());
        assert_eq!(
            parse_element("héllo wörld").unwrap().pieces(),
            &[Piece::Lit("héllo wörld".into())]
        );
    }

    #[test]
    fn unbalanced_open_errors() {
        assert_eq!(
            parse_element("x{a"),
            Err(TemplateError::Unbalanced { at: 1 })
        );
        assert_eq!(
            parse_element("x{a{b}"),
            Err(TemplateError::Unbalanced { at: 1 })
        );
        assert_eq!(parse_element("{"), Err(TemplateError::Unbalanced { at: 0 }));
    }

    #[test]
    fn unbalanced_close_errors() {
        assert_eq!(
            parse_element("ab}c"),
            Err(TemplateError::Unbalanced { at: 2 })
        );
        assert_eq!(
            parse_element("{a}}"),
            Err(TemplateError::Unbalanced { at: 3 })
        );
        assert_eq!(
            TemplateError::Unbalanced { at: 2 }.to_string(),
            "unbalanced brace at byte 2"
        );
    }

    #[test]
    fn empty_token_errors() {
        assert_eq!(
            parse_element("--x={}"),
            Err(TemplateError::EmptyToken { at: 4 })
        );
        assert_eq!(
            TemplateError::EmptyToken { at: 4 }.to_string(),
            "empty token at byte 4"
        );
    }

    #[test]
    fn bad_segment_errors() {
        for (template, seg) in [
            ("{Name}", "Name"),
            ("{a..b}", ""),
            ("{a.}", ""),
            ("{1a}", "1a"),
            ("{a-b}", "a-b"),
            ("{ a }", " a "),
            ("{_a}", "_a"),
        ] {
            assert_eq!(
                parse_element(template),
                Err(TemplateError::BadSegment {
                    at: 0,
                    seg: seg.into()
                }),
                "{template}"
            );
        }
        let ok = parse_element("{limits.cpus_int2}").unwrap();
        match ok.pieces() {
            [Piece::Tok(path)] => assert_eq!(path.segments(), ["limits", "cpus_int2"]),
            other => panic!("unexpected pieces {other:?}"),
        }
        assert!(
            TemplateError::BadSegment {
                at: 0,
                seg: "A".into()
            }
            .to_string()
            .contains("\"A\"")
        );
    }

    #[test]
    fn unknown_token_rejected_by_set() {
        let map = Map::new(&[("name", "box")]);
        let element = parse_element("--name={nme}").unwrap();
        let err = validate(&element, &map).unwrap_err();
        assert_eq!(err, TemplateError::UnknownToken { path: "nme".into() });
        assert_eq!(err.to_string(), "unknown token {nme}");
        // A token the set allows but the source cannot supply is also refused.
        let allowed_but_empty = Map::new(&[]);
        assert!(render_element(&parse_element("{name}").unwrap(), &allowed_but_empty).is_err());
        assert!(validate(&parse_element("--name={name}").unwrap(), &map).is_ok());
    }

    #[test]
    fn value_with_spaces_stays_one_element() {
        let map = Map::new(&[("worktree.path", "/home/u/my repo")]);
        assert_eq!(
            render("--workdir={worktree.path}", &map).unwrap(),
            "--workdir=/home/u/my repo"
        );
    }

    #[test]
    fn shell_syntax_characters_remain_literal_argv_content() {
        let template = r#"'two words' "quoted" C:\tmp $HOME `echo unsafe`"#;
        let element = parse_element(template).unwrap();
        assert_eq!(element.pieces(), &[Piece::Lit(template.into())]);
        assert_eq!(render_element(&element, &Map::new(&[])).unwrap(), template);
    }

    #[test]
    fn template_and_rendered_element_are_bounded() {
        let too_long = "x".repeat(MAX_TEMPLATE_ELEMENT_BYTES + 1);
        assert_eq!(
            parse_element(&too_long),
            Err(TemplateError::TooLarge {
                limit: MAX_TEMPLATE_ELEMENT_BYTES,
            })
        );
        assert_eq!(
            TemplateError::TooLarge {
                limit: MAX_TEMPLATE_ELEMENT_BYTES,
            }
            .to_string(),
            "argv template element exceeds 131072 bytes"
        );

        // Parsing is bounded by the template length; rendering is bounded by the
        // expansion, so a short template over a large value still fails.
        let at_cap = "y".repeat(MAX_TEMPLATE_ELEMENT_BYTES);
        assert_eq!(parse_element(&at_cap).unwrap().pieces().len(), 1);
        let element = parse_element("{value}").unwrap();
        let wide = Map::new(&[("value", too_long.as_str())]);
        assert_eq!(
            render_element(&element, &wide),
            Err(TemplateError::TooLarge {
                limit: MAX_TEMPLATE_ELEMENT_BYTES,
            })
        );
        let exact = Map::new(&[("value", at_cap.as_str())]);
        let rendered = render_element(&element, &exact).unwrap();
        assert_eq!(rendered.len(), MAX_TEMPLATE_ELEMENT_BYTES);
    }

    #[test]
    fn value_with_braces_not_reparsed() {
        let map = Map::new(&[("name", "{secret}}{{"), ("secret", "leaked")]);
        assert_eq!(render("{name}", &map).unwrap(), "{secret}}{{");
    }

    #[test]
    fn multiple_tokens_in_one_element() {
        let map = Map::new(&[("user", "dev"), ("host", "lab"), ("port", "2222")]);
        let element = parse_element("{user}@{host}:{port}").unwrap();
        assert_eq!(element.pieces().len(), 5);
        assert_eq!(render_element(&element, &map).unwrap(), "dev@lab:2222");
    }

    /// Property: whatever the values (spaces, quotes, braces, newlines, NULs,
    /// non-ASCII), one element renders to exactly one string equal to the
    /// literal/value concatenation. A deterministic xorshift generator stands
    /// in for proptest (not a dependency of this crate).
    #[test]
    fn render_never_splits() {
        const ALPHABET: &[char] = &[
            'a', 'Z', '0', ' ', '\t', '\n', '\0', '{', '}', '"', '\'', '\\', '$', ';', '|', 'é',
            '→', '.',
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..512 {
            let mut value = String::new();
            for _ in 0..(next() % 24) {
                value.push(ALPHABET[(next() % ALPHABET.len() as u64) as usize]);
            }
            let map = Map(BTreeMap::from([
                ("a.b", value.clone()),
                ("c", value.clone()),
            ]));
            let element = parse_element("pre {a.b} mid{c}post").unwrap();
            let rendered = render_element(&element, &map).unwrap();
            assert_eq!(rendered, format!("pre {value} mid{value}post"));
        }
    }
}
