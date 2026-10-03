//! Pure policy for launching external URLs (THE-448): which URLs may be handed
//! to the desktop opener, and how a browser command template becomes argv.
//!
//! No I/O, no shell. The host's launcher (`thegn-host::actions`) spawns what this
//! module produces.

/// Longest URL the launcher will pass on.
pub const MAX_URL_LEN: usize = 2048;
/// Longest browser command template (`$BROWSER` / `[forward] browser`).
pub const MAX_TEMPLATE_LEN: usize = 1024;

/// Why a URL was refused. Carries no URL text, so it is safe to show in status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlRejection {
    Empty,
    TooLong,
    Control,
    Invalid,
    Scheme,
    Credentials,
    NoHost,
}

impl UrlRejection {
    pub fn label(self) -> &'static str {
        match self {
            Self::Empty => "empty URL",
            Self::TooLong => "URL too long",
            Self::Control => "URL contains control characters",
            Self::Invalid => "invalid URL",
            Self::Scheme => "only http and https URLs can be opened",
            Self::Credentials => "URL embeds credentials",
            Self::NoHost => "URL has no host",
        }
    }
}

/// Parse and normalize `raw`; only bounded `http`/`https` URLs without
/// credentials pass. The result is a single argv value.
pub fn normalize_url(raw: &str) -> Result<String, UrlRejection> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(UrlRejection::Empty);
    }
    if raw.len() > MAX_URL_LEN {
        return Err(UrlRejection::TooLong);
    }
    if raw.chars().any(char::is_control) {
        return Err(UrlRejection::Control);
    }
    let u = url::Url::parse(raw).map_err(|_| UrlRejection::Invalid)?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(UrlRejection::Scheme);
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(UrlRejection::Credentials);
    }
    // Defence in depth: `url` already rejects empty hosts for http/https, so
    // no test input reaches this branch; kept in case that parser changes.
    if u.host_str().is_none_or(str::is_empty) {
        return Err(UrlRejection::NoHost);
    }
    let out: String = u.into();
    if out.len() > MAX_URL_LEN {
        return Err(UrlRejection::TooLong);
    }
    Ok(out)
}

/// Why a browser template could not be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateError {
    TooLong,
    Unterminated,
    Empty,
}

impl TemplateError {
    pub fn label(self) -> &'static str {
        match self {
            Self::TooLong => "browser command too long",
            Self::Unterminated => "browser command has an unterminated quote",
            Self::Empty => "browser command is empty",
        }
    }
}

/// Split one command into argv: whitespace-separated, `'..'` and `".."` group
/// words (so a path with spaces works). No escapes, no expansion, no shell.
fn split_words(s: &str) -> Result<Vec<String>, TemplateError> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    out.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            None => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err(TemplateError::Unterminated);
    }
    if in_word {
        out.push(cur);
    }
    Ok(out)
}

/// Parse a `$BROWSER`-style spec: colon-separated fallback commands, each an
/// argv template that may contain `%s` for the URL. Blank entries are skipped;
/// a spec with no usable entry yields `Ok(vec![])` so callers fall through to
/// the OS opener.
///
/// Note: the spec is split on ':' BEFORE quotes are considered, so a quoted
/// `$BROWSER` path that itself contains ':' is not supported.
pub fn parse_candidates(spec: &str) -> Result<Vec<Vec<String>>, TemplateError> {
    if spec.len() > MAX_TEMPLATE_LEN {
        return Err(TemplateError::TooLong);
    }
    let mut out = Vec::new();
    for entry in spec.split(':') {
        let words = split_words(entry)?;
        if !words.is_empty() {
            out.push(words);
        }
    }
    Ok(out)
}

/// Parse a single configured command (no colon splitting, so a Windows path or
/// URL-ish argument survives). Empty is an error.
pub fn parse_single(spec: &str) -> Result<Vec<String>, TemplateError> {
    if spec.len() > MAX_TEMPLATE_LEN {
        return Err(TemplateError::TooLong);
    }
    let words = split_words(spec)?;
    if words.is_empty() {
        return Err(TemplateError::Empty);
    }
    Ok(words)
}

/// Build the final argv: every `%s` in every word becomes the URL; with no
/// `%s` anywhere the URL is appended as one trailing argument. The URL is
/// never split.
pub fn build_argv(template: &[String], url: &str) -> Vec<String> {
    if template.iter().any(|w| w.contains("%s")) {
        template.iter().map(|w| w.replace("%s", url)).collect()
    } else {
        let mut v = template.to_vec();
        v.push(url.to_owned());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_and_normalizes_http() {
        assert_eq!(
            normalize_url(" https://Example.com/a b ").unwrap(),
            "https://example.com/a%20b"
        );
        assert_eq!(
            normalize_url("http://localhost:3000").unwrap(),
            "http://localhost:3000/"
        );
    }

    #[test]
    fn rejects_bad_urls() {
        use UrlRejection::*;
        assert_eq!(normalize_url("  "), Err(Empty));
        assert_eq!(normalize_url("file:///etc/passwd"), Err(Scheme));
        assert_eq!(normalize_url("javascript:alert(1)"), Err(Scheme));
        assert_eq!(normalize_url("mailto:a@b.c"), Err(Scheme));
        assert_eq!(normalize_url("https://u:p@example.com/"), Err(Credentials));
        assert_eq!(normalize_url("https://u@example.com/"), Err(Credentials));
        assert_eq!(normalize_url("https://exa\nmple.com"), Err(Control));
        assert_eq!(normalize_url("not a url"), Err(Invalid));
        assert_eq!(normalize_url("https://"), Err(Invalid));
        let long = format!("https://e.com/{}", "a".repeat(MAX_URL_LEN));
        assert_eq!(normalize_url(&long), Err(TooLong));
        for r in [
            Empty,
            TooLong,
            Control,
            Invalid,
            Scheme,
            Credentials,
            NoHost,
        ] {
            assert!(!r.label().is_empty());
        }
    }

    #[test]
    fn candidates_split_on_colon_with_args_and_quotes() {
        let c = parse_candidates("firefox --new-window %s: lynx :\"/opt/my br/bin\" -x").unwrap();
        assert_eq!(
            c,
            vec![
                vec!["firefox", "--new-window", "%s"],
                vec!["lynx"],
                vec!["/opt/my br/bin", "-x"],
            ]
        );
        assert!(parse_candidates(" : ").unwrap().is_empty());
        assert_eq!(parse_candidates("a 'b"), Err(TemplateError::Unterminated));
        assert_eq!(
            parse_candidates(&"a".repeat(MAX_TEMPLATE_LEN + 1)),
            Err(TemplateError::TooLong)
        );
        assert_eq!(parse_candidates("''").unwrap(), vec![vec![String::new()]]);
    }

    #[test]
    fn single_command_keeps_colons() {
        assert_eq!(
            parse_single("C:\\b\\br.exe --x").unwrap(),
            vec!["C:\\b\\br.exe", "--x"]
        );
        assert_eq!(parse_single("  "), Err(TemplateError::Empty));
        assert_eq!(
            parse_single(&"a".repeat(MAX_TEMPLATE_LEN + 1)),
            Err(TemplateError::TooLong)
        );
        for e in [
            TemplateError::TooLong,
            TemplateError::Unterminated,
            TemplateError::Empty,
        ] {
            assert!(!e.label().is_empty());
        }
    }

    #[test]
    fn argv_substitutes_or_appends_without_splitting() {
        let url = "https://e.com/?a=1&b=2%203";
        let t = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            build_argv(&t(&["ff", "--new-window", "%s"]), url),
            vec!["ff", "--new-window", url]
        );
        assert_eq!(
            build_argv(&t(&["ff", "--u=%s"]), url),
            vec!["ff".to_string(), format!("--u={url}")]
        );
        assert_eq!(build_argv(&t(&["xdg-open"]), url), vec!["xdg-open", url]);
    }
}
