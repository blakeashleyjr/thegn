//! Native token-reduction engine.
//!
//! There is no `rtk` crate (despite the roadmap listing it as one) — this is the
//! from-scratch implementation of group **W**. It shrinks noisy tool/command
//! output before it is billed/processed by the model, via a stack of small,
//! deterministic transforms layered by aggressiveness.
//!
//! **Determinism is the cache-safety contract.** The proxy applies this to the
//! same re-sent tool output on every turn; because [`compress`] is a pure
//! function, the upstream sees byte-identical compressed content turn-over-turn,
//! so prompt caching holds. (`compress(x)` is also idempotent: compressing an
//! already-compressed block is a no-op.)

use std::sync::LazyLock;

use regex::Regex;

/// Compression aggressiveness. Higher levels add lossier transforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Level {
    /// No compression.
    Off,
    /// Lossless-ish: strip ANSI, fold progress redraws, trim/again-collapse blanks.
    #[default]
    Conservative,
    /// + collapse repeated lines, minify whole-block JSON.
    Balanced,
    /// + collapse intra-line whitespace and head/tail-truncate huge blocks.
    Aggressive,
}

impl Level {
    fn at_least(self, other: Level) -> bool {
        self as u8 >= other as u8
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Off => "off",
            Level::Conservative => "conservative",
            Level::Balanced => "balanced",
            Level::Aggressive => "aggressive",
        }
    }
    /// Parses a config string, defaulting to `conservative` for unknown input.
    pub fn parse(s: &str) -> Level {
        match s.trim().to_lowercase().as_str() {
            "off" | "none" | "false" => Level::Off,
            "balanced" => Level::Balanced,
            "aggressive" => Level::Aggressive,
            _ => Level::Conservative,
        }
    }
}

/// Head/tail truncation limits for [`Level::Aggressive`].
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Blocks longer than this (chars) are truncated.
    pub max_block_chars: usize,
    pub keep_head: usize,
    pub keep_tail: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_block_chars: 8000,
            keep_head: 2000,
            keep_tail: 2000,
        }
    }
}

/// The compressed text plus how many chars were removed (>= 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressResult {
    pub text: String,
    pub saved_chars: usize,
}

/// Compresses `input` at `level`. Pure and deterministic.
pub fn compress(input: &str, level: Level, limits: Limits) -> CompressResult {
    if level == Level::Off {
        return CompressResult {
            text: input.to_string(),
            saved_chars: 0,
        };
    }
    let original = input.chars().count();

    let mut s = strip_ansi(input);
    s = fold_carriage_returns(&s);
    s = trim_and_collapse_blank_lines(&s);

    if level.at_least(Level::Balanced) {
        match minify_json_block(&s) {
            Some(min) => s = min,
            None => s = dedup_repeated_lines(&s),
        }
    }

    if level.at_least(Level::Aggressive) {
        s = collapse_intra_line_ws(&s);
        s = truncate_middle(&s, limits);
    }

    let new = s.chars().count();
    CompressResult {
        text: s,
        saved_chars: original.saturating_sub(new),
    }
}

// ── Transforms (each pure) ──────────────────────────────────────────────────

// CSI (`ESC [ … final`) and OSC (`ESC ] … BEL/ST`) escape sequences.
static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]")
        .unwrap()
});

/// Removes ANSI/VT escape sequences (color, cursor moves, OSC titles).
pub fn strip_ansi(s: &str) -> String {
    ANSI_RE.replace_all(s, "").into_owned()
}

/// Normalizes CRLF, then folds carriage-return progress redraws: a line rewritten
/// with `\r` keeps only its final visible state (the text after the last `\r`).
pub fn fold_carriage_returns(s: &str) -> String {
    let normalized = s.replace("\r\n", "\n");
    normalized
        .split('\n')
        .map(|line| match line.rfind('\r') {
            Some(_) => line.rsplit('\r').next().unwrap_or(line),
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Trims trailing whitespace per line and collapses runs of 3+ blank lines to one.
pub fn trim_and_collapse_blank_lines(s: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut blank_run = 0usize;
    // Trim trailing whitespace first (into owned lines), then collapse.
    let trimmed: Vec<String> = s.split('\n').map(|l| l.trim_end().to_string()).collect();
    for line in &trimmed {
        if line.is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push(line);
            }
        } else {
            blank_run = 0;
            out.push(line);
        }
    }
    out.join("\n")
}

/// Collapses runs of 4+ identical consecutive lines to one line plus a marker.
pub fn dedup_repeated_lines(s: &str) -> String {
    let lines: Vec<&str> = s.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let mut run = 1;
        while i + run < lines.len() && lines[i + run] == line {
            run += 1;
        }
        out.push(line.to_string());
        if run >= 4 {
            out.push(format!("… ({} identical lines omitted) …", run - 1));
        } else {
            // Emit the remaining duplicates verbatim (short run).
            for _ in 1..run {
                out.push(line.to_string());
            }
        }
        i += run;
    }
    out.join("\n")
}

/// If the whole trimmed block is valid JSON, re-serializes it compactly.
pub fn minify_json_block(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let compact = serde_json::to_string(&v).ok()?;
    // Only worth it if it actually shrank.
    (compact.len() < s.len()).then_some(compact)
}

/// Collapses runs of horizontal whitespace (multiple spaces, or any tab) within
/// each line to a single space. Lossy on indentation — Aggressive only.
pub fn collapse_intra_line_ws(s: &str) -> String {
    static WS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?: {2,}|\t+)").unwrap());
    s.split('\n')
        .map(|line| WS_RE.replace_all(line, " ").into_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Head/tail truncation for blocks over `limits.max_block_chars`: keeps the first
/// `keep_head` and last `keep_tail` chars with an elision marker between.
pub fn truncate_middle(s: &str, limits: Limits) -> String {
    let chars: Vec<char> = s.chars().collect();
    let total = chars.len();
    if total <= limits.max_block_chars || total <= limits.keep_head + limits.keep_tail {
        return s.to_string();
    }
    let head: String = chars[..limits.keep_head].iter().collect();
    let tail: String = chars[total - limits.keep_tail..].iter().collect();
    let elided = total - limits.keep_head - limits.keep_tail;
    format!("{head}\n… ({elided} chars elided) …\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lim() -> Limits {
        Limits::default()
    }

    #[test]
    fn off_is_identity() {
        let r = compress("a\x1b[31mb", Level::Off, lim());
        assert_eq!(r.text, "a\x1b[31mb");
        assert_eq!(r.saved_chars, 0);
    }

    #[test]
    fn strips_ansi_color_and_osc() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("\x1b[1;32mok\x1b[m done"), "ok done");
        // OSC title set, BEL-terminated.
        assert_eq!(strip_ansi("\x1b]0;title\x07text"), "text");
    }

    #[test]
    fn folds_progress_redraws() {
        // A progress bar rewriting one line keeps only the final state.
        assert_eq!(fold_carriage_returns("10%\r50%\r100%"), "100%");
        // CRLF is normalized, not treated as a redraw.
        assert_eq!(fold_carriage_returns("a\r\nb"), "a\nb");
    }

    #[test]
    fn collapses_blank_lines_and_trailing_ws() {
        assert_eq!(trim_and_collapse_blank_lines("a  \n\n\n\n\nb"), "a\n\nb");
    }

    #[test]
    fn dedups_long_runs_only() {
        let input = "x\nx\nx\nx\nx\ny";
        // run of 5 → one line + "(4 identical lines omitted)".
        let out = dedup_repeated_lines(input);
        assert!(out.contains("4 identical lines omitted"));
        assert_eq!(
            out.matches("x").count(),
            1 + "(4 identical lines omitted)".matches('x').count()
        );
        // a short run of 2 is left verbatim.
        assert_eq!(dedup_repeated_lines("a\na\nb"), "a\na\nb");
    }

    #[test]
    fn minifies_json_blocks() {
        let pretty = "{\n  \"a\": 1,\n  \"b\": [1, 2, 3]\n}";
        let min = minify_json_block(pretty).unwrap();
        assert_eq!(min, "{\"a\":1,\"b\":[1,2,3]}");
        assert!(minify_json_block("not json").is_none());
        assert!(minify_json_block("plain text { not json").is_none());
    }

    #[test]
    fn collapses_intra_line_whitespace() {
        assert_eq!(collapse_intra_line_ws("a      b\tc"), "a b c");
    }

    #[test]
    fn truncates_middle_of_huge_blocks() {
        let limits = Limits {
            max_block_chars: 10,
            keep_head: 3,
            keep_tail: 3,
        };
        let out = truncate_middle("0123456789abcdef", limits);
        assert!(out.starts_with("012"));
        assert!(out.ends_with("def"));
        assert!(out.contains("chars elided"));
        // Under the limit → untouched.
        assert_eq!(truncate_middle("short", limits), "short");
    }

    #[test]
    fn conservative_pipeline() {
        let input = "\x1b[32mbuild\x1b[0m\n10%\r100%\n\n\n\n\ndone   ";
        let r = compress(input, Level::Conservative, lim());
        assert_eq!(r.text, "build\n100%\n\ndone");
        assert!(r.saved_chars > 0);
    }

    #[test]
    fn balanced_minifies_json() {
        let input = "{\n  \"k\": \"v\"\n}";
        let r = compress(input, Level::Balanced, lim());
        assert_eq!(r.text, "{\"k\":\"v\"}");
    }

    #[test]
    fn deterministic_and_idempotent() {
        let input = "\x1b[31mERR\x1b[0m\nsame\nsame\nsame\nsame\nsame\n{\n  \"a\": 1\n}";
        for level in [Level::Conservative, Level::Balanced, Level::Aggressive] {
            let once = compress(input, level, lim());
            // Deterministic: same input → same output.
            assert_eq!(once, compress(input, level, lim()));
            // Idempotent: compressing the output again is a no-op (cache stability).
            let twice = compress(&once.text, level, lim());
            assert_eq!(twice.text, once.text, "level {level:?} not idempotent");
        }
    }

    #[test]
    fn level_parse_and_str() {
        assert_eq!(Level::parse("off"), Level::Off);
        assert_eq!(Level::parse("BALANCED"), Level::Balanced);
        assert_eq!(Level::parse("aggressive"), Level::Aggressive);
        assert_eq!(Level::parse("nonsense"), Level::Conservative);
        assert_eq!(Level::Balanced.as_str(), "balanced");
    }
}
