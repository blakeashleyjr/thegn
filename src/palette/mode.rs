//! Prefix routing for the palette's single input. The first character(s) pick a
//! source; the remainder is the query handed to that source. No prefix is the
//! "Smart" default (frecency-ranked commands + nav targets). Backspacing past a
//! prefix naturally falls back to Smart, since the prefix simply disappears.

/// Which source the current input is querying.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No prefix: frecency-ranked commands + nav, blended.
    Smart,
    /// `>` — the full action catalog (a live cheatsheet).
    Command,
    /// `@` — worktrees, repos (recents), open tabs.
    Nav,
    /// `f ` / `.` — fuzzy file finder in the focused worktree.
    File,
    /// `/` or `rg ` — embedded ripgrep content search.
    Content,
    /// `g ` — branches + PR/diff actions.
    Git,
}

impl Mode {
    /// Short label for the footer mode pill.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Smart => "SMART",
            Mode::Command => "CMD",
            Mode::Nav => "NAV",
            Mode::File => "FILE",
            Mode::Content => "GREP",
            Mode::Git => "GIT",
        }
    }

    /// The accent hue (theme "R;G;B") that tints this mode's chrome.
    pub fn hue(self) -> &'static str {
        match self {
            Mode::Smart => crate::theme::TEAL,
            Mode::Command => crate::theme::TEAL,
            Mode::Nav => crate::theme::PURPLE,
            Mode::File => crate::theme::BLUE,
            Mode::Content => crate::theme::AMBER,
            Mode::Git => crate::theme::GREEN,
        }
    }
}

/// A parsed input: the routed mode and the residual query (prefix stripped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub mode: Mode,
    pub query: String,
}

/// Route raw input to a (mode, query). Space-terminated word prefixes (`f `,
/// `g `, `rg `) require the space so single letters still fuzzy-search in Smart
/// mode; sigil prefixes (`>`, `@`, `/`, `.`) bind immediately.
pub fn parse(input: &str) -> Parsed {
    let rest = |s: &str, n: usize| s[n..].to_string();
    if let Some(q) = input.strip_prefix('>') {
        Parsed {
            mode: Mode::Command,
            query: q.to_string(),
        }
    } else if let Some(q) = input.strip_prefix('@') {
        Parsed {
            mode: Mode::Nav,
            query: q.to_string(),
        }
    } else if let Some(q) = input.strip_prefix('/') {
        Parsed {
            mode: Mode::Content,
            query: q.to_string(),
        }
    } else if let Some(q) = input.strip_prefix("rg ") {
        Parsed {
            mode: Mode::Content,
            query: q.to_string(),
        }
    } else if let Some(q) = input.strip_prefix("f ") {
        Parsed {
            mode: Mode::File,
            query: q.to_string(),
        }
    } else if let Some(q) = input.strip_prefix("g ") {
        Parsed {
            mode: Mode::Git,
            query: q.to_string(),
        }
    } else if input.starts_with('.') {
        // `.` binds immediately (a leading dot is rarely a Smart query) so file
        // search feels instant; the dot itself is dropped from the query.
        Parsed {
            mode: Mode::File,
            query: rest(input, 1),
        }
    } else {
        Parsed {
            mode: Mode::Smart,
            query: input.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> (Mode, &'static str) {
        let parsed = parse(s);
        // leak only in test for ergonomic comparison
        (parsed.mode, Box::leak(parsed.query.into_boxed_str()))
    }

    #[test]
    fn smart_is_default() {
        assert_eq!(p("toggle"), (Mode::Smart, "toggle"));
        assert_eq!(p(""), (Mode::Smart, ""));
    }

    #[test]
    fn sigil_prefixes_bind_immediately() {
        assert_eq!(p(">tog"), (Mode::Command, "tog"));
        assert_eq!(p("@auth"), (Mode::Nav, "auth"));
        assert_eq!(p("/useEffect"), (Mode::Content, "useEffect"));
        assert_eq!(p(".main"), (Mode::File, "main"));
    }

    #[test]
    fn word_prefixes_need_a_space() {
        assert_eq!(p("f main.rs"), (Mode::File, "main.rs"));
        assert_eq!(p("g merge"), (Mode::Git, "merge"));
        assert_eq!(p("rg TODO"), (Mode::Content, "TODO"));
        // Bare letters still fuzzy-search in Smart mode (no premature routing).
        assert_eq!(p("f"), (Mode::Smart, "f"));
        assert_eq!(p("g"), (Mode::Smart, "g"));
        assert_eq!(p("grep"), (Mode::Smart, "grep"));
    }

    #[test]
    fn backspacing_past_a_prefix_returns_to_smart() {
        // Simulate deleting the leading '>' — the residual is plain Smart text.
        assert_eq!(parse(">").mode, Mode::Command);
        assert_eq!(parse("").mode, Mode::Smart);
    }
}
