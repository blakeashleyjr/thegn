//! Candidate policy: what a completion candidate is, and the pure pipeline
//! every source's output passes through before it reaches a shell.
//!
//! ## The sanitisation choice, and why
//!
//! Every shell protocol `clap_complete` speaks is line-oriented with an
//! in-line separator between the value and its description — `value\tdesc` for
//! PowerShell, `value:desc` for zsh, one bare value per line for bash and fish.
//! A value carrying a newline or a tab therefore does not render badly, it
//! **desynchronises the parse**: one candidate becomes two, or a description
//! becomes a selectable value. There is no escaping that all five shells agree
//! on, so this module does not try to invent one.
//!
//! - **Values with a control character are dropped**, not escaped. A worktree
//!   or branch whose name contains a newline is pathological, and silently
//!   omitting it from a `<TAB>` list is strictly better than corrupting the
//!   whole list. [`sanitize_value`] is the predicate.
//! - **Descriptions are sanitised, not dropped.** They are cosmetic: a control
//!   character becomes a space, runs collapse, and the result is truncated to
//!   [`MAX_DESCRIPTION_CHARS`]. Losing a description must never lose the value
//!   it describes.
//!
//! Truncation counts **chars, not display columns** — this crate has no
//! display-width dependency, and the number is a politeness bound rather than a
//! layout constraint. It always cuts on a char boundary (a byte-index cut is
//! the classic multi-byte panic; `truncates_on_a_char_boundary` pins it).

/// Hard ceiling on how many candidates one request may emit. A shell handed
/// 5000 candidates is useless — it paginates them into a wall of text and takes
/// visible time to render. Sources that can exceed this are expected to be
/// filtered first (see [`refine`]) so the cap trims the tail, not the match.
pub const MAX_CANDIDATES: usize = 200;

/// Description length ceiling, in chars. Long enough for a branch subject or a
/// capability summary, short enough that zsh's two-column listing stays legible.
pub const MAX_DESCRIPTION_CHARS: usize = 60;

/// The ellipsis appended to a truncated description.
const ELLIPSIS: char = '…';

/// One completion candidate: the value the shell would insert, plus an optional
/// one-line description shells that support it (zsh, fish, PowerShell) display
/// alongside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub value: String,
    pub description: Option<String>,
}

impl Candidate {
    /// A bare candidate with no description.
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            description: None,
        }
    }

    /// A candidate with a description. An empty description is treated as none,
    /// so callers can pass a possibly-empty DB column without a guard.
    pub fn described(value: impl Into<String>, description: impl Into<String>) -> Self {
        let description = description.into();
        Self {
            value: value.into(),
            description: (!description.trim().is_empty()).then_some(description),
        }
    }
}

/// Whether a value is safe to put on the wire. Rejects any control character —
/// newline and tab are the protocol separators, and the rest (NUL, escape,
/// carriage return) have no business in a shell word either.
pub fn sanitize_value(value: &str) -> bool {
    !value.is_empty() && !value.chars().any(char::is_control)
}

/// Flatten a description to one clean line: control characters become spaces,
/// runs of whitespace collapse, and the result is trimmed and truncated to
/// [`MAX_DESCRIPTION_CHARS`]. Returns `None` if nothing legible survives.
pub fn sanitize_description(description: &str) -> Option<String> {
    let flattened: String = description
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let mut out = String::with_capacity(flattened.len());
    for word in flattened.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    (!out.is_empty()).then(|| truncate_description(&out, MAX_DESCRIPTION_CHARS))
}

/// Truncate to at most `max` chars, appending an ellipsis when anything was
/// cut. Always cuts on a char boundary.
pub fn truncate_description(description: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if description.chars().count() <= max {
        return description.to_string();
    }
    // `max - 1` chars plus the ellipsis, so the rendered width stays within
    // `max`. Built from `chars()` rather than a byte slice — the whole point.
    let mut out: String = description.chars().take(max.saturating_sub(1)).collect();
    // A cut mid-word usually lands on a space; do not leave a dangling one.
    while out.ends_with(char::is_whitespace) {
        out.pop();
    }
    out.push(ELLIPSIS);
    out
}

/// The full candidate pipeline, in order:
///
/// 1. drop values that would corrupt the wire ([`sanitize_value`]);
/// 2. keep only values that are a byte-prefix match for `current` — shells
///    expect prefix semantics, not fuzzy ones (the fuzzy surface is the palette,
///    which is a different product surface with its own spec);
/// 3. de-duplicate by value, **first occurrence wins**, order otherwise stable —
///    a source's own ordering (recency, config order) is meaningful;
/// 4. sanitise descriptions;
/// 5. cap at [`MAX_CANDIDATES`].
pub fn refine(raw: impl IntoIterator<Item = Candidate>, current: &str) -> Vec<Candidate> {
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<Candidate> = Vec::new();
    for candidate in raw {
        if out.len() >= MAX_CANDIDATES {
            break;
        }
        if !sanitize_value(&candidate.value) || !candidate.value.starts_with(current) {
            continue;
        }
        if !seen.insert(candidate.value.clone()) {
            continue;
        }
        out.push(Candidate {
            description: candidate
                .description
                .as_deref()
                .and_then(sanitize_description),
            value: candidate.value,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(cs: &[Candidate]) -> Vec<&str> {
        cs.iter().map(|c| c.value.as_str()).collect()
    }

    #[test]
    fn described_treats_blank_as_none() {
        assert_eq!(Candidate::new("a").description, None);
        assert_eq!(Candidate::described("a", "").description, None);
        assert_eq!(Candidate::described("a", "   ").description, None);
        assert_eq!(
            Candidate::described("a", "hi").description.as_deref(),
            Some("hi")
        );
    }

    #[test]
    fn values_with_control_characters_are_rejected() {
        assert!(sanitize_value("tg/feature-1"));
        assert!(sanitize_value("has spaces"));
        assert!(sanitize_value("ünïcøde"));
        assert!(!sanitize_value(""));
        assert!(!sanitize_value("two\nlines"));
        assert!(!sanitize_value("a\tb"));
        assert!(!sanitize_value("a\rb"));
        assert!(!sanitize_value("nul\0byte"));
        assert!(!sanitize_value("esc\x1b[31m"));
    }

    #[test]
    fn descriptions_are_flattened_not_dropped() {
        assert_eq!(sanitize_description("one\ntwo").as_deref(), Some("one two"));
        assert_eq!(
            sanitize_description("  spaced \t out  ").as_deref(),
            Some("spaced out")
        );
        assert_eq!(sanitize_description(""), None);
        assert_eq!(sanitize_description("\n\t "), None);
    }

    #[test]
    fn truncates_on_a_char_boundary() {
        // Every char here is multi-byte: a byte-index cut would panic.
        let wide = "ドキュメントのタイトルはとても長いことがあります";
        let cut = truncate_description(wide, 5);
        assert_eq!(cut.chars().count(), 5);
        assert!(cut.ends_with(ELLIPSIS));
        assert!(wide.starts_with(&cut[..cut.len() - ELLIPSIS.len_utf8()]));

        // Combining marks / emoji, same deal.
        let emoji = "🌊🌊🌊🌊🌊🌊";
        assert_eq!(truncate_description(emoji, 3).chars().count(), 3);

        // Short enough: unchanged, no ellipsis.
        assert_eq!(truncate_description("short", 10), "short");
        assert_eq!(truncate_description("exactly10!", 10), "exactly10!");
        // Degenerate width.
        assert_eq!(truncate_description("anything", 0), "");
        assert_eq!(truncate_description("anything", 1), "…");
        // A cut landing on a space does not leave one dangling.
        assert_eq!(truncate_description("ab cdef", 4), "ab…");
    }

    #[test]
    fn long_descriptions_are_capped_by_the_pipeline() {
        let long = "x".repeat(500);
        let out = refine([Candidate::described("v", long)], "");
        assert_eq!(
            out[0].description.as_ref().unwrap().chars().count(),
            MAX_DESCRIPTION_CHARS
        );
    }

    #[test]
    fn refine_prefix_matches_by_byte_prefix() {
        let raw = [
            Candidate::new("alpha"),
            Candidate::new("alpine"),
            Candidate::new("beta"),
            Candidate::new("Alpha"),
        ];
        assert_eq!(values(&refine(raw.clone(), "al")), ["alpha", "alpine"]);
        // Case-sensitive, and not fuzzy: "aph" matches nothing.
        assert_eq!(values(&refine(raw.clone(), "A")), ["Alpha"]);
        assert!(refine(raw.clone(), "aph").is_empty());
        // An empty prefix keeps everything.
        assert_eq!(refine(raw, "").len(), 4);
    }

    #[test]
    fn refine_dedups_stably_first_wins() {
        let out = refine(
            [
                Candidate::described("a", "first"),
                Candidate::new("b"),
                Candidate::described("a", "second"),
                Candidate::new("c"),
            ],
            "",
        );
        assert_eq!(values(&out), ["a", "b", "c"]);
        assert_eq!(out[0].description.as_deref(), Some("first"));
    }

    #[test]
    fn refine_drops_hostile_values_but_keeps_the_rest() {
        let out = refine(
            [
                Candidate::new("good"),
                Candidate::new("go\nod"),
                Candidate::new("go\tod"),
                Candidate::described("golden", "a\ndescription"),
            ],
            "go",
        );
        assert_eq!(values(&out), ["good", "golden"]);
        assert_eq!(out[1].description.as_deref(), Some("a description"));
    }

    #[test]
    fn refine_caps_the_output() {
        let many: Vec<Candidate> = (0..MAX_CANDIDATES * 3)
            .map(|i| Candidate::new(format!("v{i:05}")))
            .collect();
        let out = refine(many, "v");
        assert_eq!(out.len(), MAX_CANDIDATES);
        // The cap trims the tail — the first candidates a source ranked are kept.
        assert_eq!(out[0].value, "v00000");
    }

    #[test]
    fn refine_filters_before_capping() {
        // 300 non-matches followed by one match: the match must still surface,
        // i.e. the cap counts *kept* candidates, not inspected ones.
        let mut raw: Vec<Candidate> = (0..300).map(|i| Candidate::new(format!("x{i}"))).collect();
        raw.push(Candidate::new("wanted"));
        assert_eq!(values(&refine(raw, "wan")), ["wanted"]);
    }
}
