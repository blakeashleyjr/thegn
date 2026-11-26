//! Cleanup of finished dictation text before it reaches a pane.
//!
//! Pure and I/O-free. The final transcript pipeline is [`finalize`]: strip
//! non-speech annotations and subtitle credits, then drop a vocabulary-prompt
//! echo, then apply the replacement rules.
//!
//! Whisper emits annotations such as `[BLANK_AUDIO]` or `(music)` for
//! non-speech audio, and often ends a transcript with a subtitle credit that
//! was never spoken; `strip_non_speech` removes those.
//!
//! Replacement rules are case-insensitive literal phrases matched only on word
//! boundaries. Overlapping matches are resolved longest-first, then by earlier
//! start, then by earlier rule. This is deliberately simpler than Hex: no
//! re-capitalization and no punctuation clean-up beyond one adjacent space.

use regex::Regex;

use crate::config_voice::{MAX_REPLACEMENTS, VoiceReplacement};

/// Annotation words that mark non-speech audio, lowercased with underscores
/// read as spaces.
const NON_SPEECH: &[&str] = &[
    "blank audio",
    "silence",
    "music",
    "applause",
    "laughter",
    "noise",
    "inaudible",
    "no speech",
    "sound",
    "upbeat music",
];

/// Trailing subtitle credits in English. Matched after dropping every
/// non-alphanumeric character and lowercasing.
const ENGLISH_CREDITS: &[&str] = &[
    "Thanks for watching!",
    "Thank you for watching.",
    "Subtitles by the Amara.org community",
];

/// Trailing subtitle credits that Whisper emits for non-English audio.
const FOREIGN_CREDITS: &[&str] = &[
    "Merci d'avoir regardé cette vidéo !",
    "Sous-titres réalisés par la communauté d'Amara.org",
    "Altyazı M.K.",
    "Bu videoyu izlediğiniz için teşekkür ederim",
    "Продолжение следует",
    "Спасибо за просмотр!",
    "Субтитры сделал DimaTorzok",
];

/// Remove non-speech annotations and one trailing subtitle credit.
///
/// Square-bracket spans are removed when their inner text starts with a
/// non-speech word. Parenthesized spans are removed only when that word ends
/// at the span's end or at a non-alphanumeric character, so `(see above)`
/// stays. The English credits always apply. The foreign credits apply when
/// `language` is not `en`. Only one credit is removed, and only from the end.
/// Returns the input unchanged when nothing matches.
pub fn strip_non_speech(text: &str, language: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut removed = false;
    let mut rest = text;
    while let Some(pos) = rest.find(['[', '(']) {
        out.push_str(&rest[..pos]);
        let open = rest[pos..].chars().next().unwrap_or('[');
        let close = if open == '[' { ']' } else { ')' };
        let inner_start = pos + open.len_utf8();
        let Some(rel) = rest[inner_start..].find(close) else {
            // Unclosed opener: keep it literally and keep scanning after it.
            out.push_str(&rest[pos..inner_start]);
            rest = &rest[inner_start..];
            continue;
        };
        let inner_end = inner_start + rel;
        let after = inner_end + close.len_utf8();
        let inner = &rest[inner_start..inner_end];
        if is_non_speech(inner, open == '(') {
            while out.ends_with(' ') {
                out.pop();
            }
            removed = true;
        } else {
            out.push_str(&rest[pos..after]);
        }
        rest = &rest[after..];
    }
    out.push_str(rest);

    let credits = credit_list(language);
    if let Some(cut) = trailing_credit_start(&out, &credits) {
        out.truncate(cut);
        removed = true;
    }
    if !removed {
        return out;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether an inner bracket text is a non-speech annotation.
fn is_non_speech(inner: &str, word_bounded: bool) -> bool {
    let norm = normalize_words(inner);
    NON_SPEECH.iter().any(|word| {
        if norm == *word {
            return true;
        }
        match norm.strip_prefix(*word) {
            Some(tail) if word_bounded => tail.chars().next().is_some_and(|c| !c.is_alphanumeric()),
            Some(_) => true,
            None => false,
        }
    })
}

/// Lowercase, read underscores as spaces, and collapse whitespace.
fn normalize_words(text: &str) -> String {
    text.to_lowercase()
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn credit_list(language: &str) -> Vec<String> {
    let mut credits: Vec<&str> = ENGLISH_CREDITS.to_vec();
    if !language.trim().eq_ignore_ascii_case("en") {
        credits.extend_from_slice(FOREIGN_CREDITS);
    }
    credits.into_iter().map(alnum_lower).collect()
}

/// Keep only alphanumeric characters, lowercased.
fn alnum_lower(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Byte index where a trailing credit starts, if `text` ends with one at a
/// word boundary.
fn trailing_credit_start(text: &str, credits: &[String]) -> Option<usize> {
    let max_len = credits.iter().map(String::len).max()?;
    for (index, _) in text.char_indices().rev() {
        // Build at most `max_len + 1` characters so the walk stays bounded.
        let suffix: String = text[index..]
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .take(max_len + 1)
            .collect();
        if suffix.len() > max_len {
            break;
        }
        if credits.iter().any(|credit| *credit == suffix) {
            let at_boundary = text[..index]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric());
            if at_boundary {
                let kept = text[..index].trim_end();
                return Some(kept.len());
            }
        }
    }
    None
}

/// Characters from scripts written without spaces. They count as word
/// boundaries, so a phrase can match inside CJK, Thai, or Hangul text.
fn is_script_boundary(c: char) -> bool {
    matches!(
        c,
        '\u{1100}'..='\u{11FF}'        // Hangul Jamo
            | '\u{0E00}'..='\u{0E7F}'  // Thai
            | '\u{3040}'..='\u{309F}'  // Hiragana
            | '\u{30A0}'..='\u{30FF}'  // Katakana
            | '\u{3130}'..='\u{318F}'  // Hangul compatibility Jamo
            | '\u{3400}'..='\u{4DBF}'  // CJK extension A
            | '\u{4E00}'..='\u{9FFF}'  // CJK unified ideographs
            | '\u{AC00}'..='\u{D7AF}'  // Hangul syllables
            | '\u{F900}'..='\u{FAFF}'  // CJK compatibility ideographs
            | '\u{20000}'..='\u{2FA1F}' // CJK extensions B onward
    )
}

/// A character that makes a neighbouring phrase part of a word.
fn is_word_char(c: char) -> bool {
    (c.is_alphanumeric() || c == '_') && !is_script_boundary(c)
}

struct Compiled {
    pattern: Regex,
    to: String,
}

/// One candidate match: byte span plus the index of the rule that produced it.
#[derive(Clone, Copy)]
struct Candidate {
    start: usize,
    end: usize,
    rule: usize,
}

/// Compiled replacement rules. Build once per configuration and reuse.
pub struct Replacements {
    rules: Vec<Compiled>,
}

impl Replacements {
    /// Compile the rules. Empty `from` entries are skipped, and only the first
    /// [`MAX_REPLACEMENTS`] rules are kept.
    pub fn new(rules: &[VoiceReplacement]) -> Self {
        let rules: Vec<Compiled> = rules
            .iter()
            .filter(|rule| !rule.from.is_empty())
            .take(MAX_REPLACEMENTS)
            .filter_map(|rule| {
                let pattern = Regex::new(&format!("(?i){}", regex::escape(&rule.from))).ok()?;
                Some(Compiled {
                    pattern,
                    to: rule.to.clone(),
                })
            })
            .collect();
        Self { rules }
    }

    /// Apply every rule to `text` and return the rewritten text.
    pub fn apply(&self, text: &str) -> String {
        if self.rules.is_empty() {
            return text.to_owned();
        }
        let mut candidates = Vec::new();
        for (rule, compiled) in self.rules.iter().enumerate() {
            collect_matches(&compiled.pattern, text, rule, &mut candidates);
        }
        // Longest first, then earlier start, then earlier rule.
        candidates.sort_by(|a, b| {
            (b.end - b.start)
                .cmp(&(a.end - a.start))
                .then(a.start.cmp(&b.start))
                .then(a.rule.cmp(&b.rule))
        });
        let mut accepted: Vec<Candidate> = Vec::new();
        for candidate in candidates {
            let overlaps = accepted
                .iter()
                .any(|kept| candidate.start < kept.end && kept.start < candidate.end);
            if !overlaps {
                accepted.push(candidate);
            }
        }
        accepted.sort_by_key(|candidate| candidate.start);

        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for candidate in &accepted {
            out.push_str(&text[cursor..candidate.start]);
            let to = &self.rules[candidate.rule].to;
            if to.is_empty() {
                // Deletion drops one adjacent space: the following one, or the
                // preceding one at the end of a sentence. Never consume a space
                // that belongs to another accepted match.
                let next_is_match = accepted.iter().any(|other| other.start == candidate.end);
                if !next_is_match && text[candidate.end..].starts_with(' ') {
                    cursor = candidate.end + 1;
                } else {
                    cursor = candidate.end;
                    if out.ends_with(' ') {
                        out.pop();
                    }
                }
            } else {
                out.push_str(to);
                cursor = candidate.end;
            }
        }
        out.push_str(&text[cursor..]);
        out
    }
}

/// Find every boundary-respecting match of one rule.
fn collect_matches(pattern: &Regex, text: &str, rule: usize, out: &mut Vec<Candidate>) {
    let mut position = 0;
    while position <= text.len() {
        let Some(found) = pattern.find_at(text, position) else {
            break;
        };
        let (start, end) = (found.start(), found.end());
        let before_ok = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word_char(c));
        let after_ok = text[end..].chars().next().is_none_or(|c| !is_word_char(c));
        if before_ok && after_ok {
            out.push(Candidate { start, end, rule });
            position = end;
        } else {
            // Retry one character later so an overlapping valid match is not lost.
            position = start + text[start..].chars().next().map_or(1, char::len_utf8);
        }
    }
}

/// Apply `rules` to a finished transcript. Returns the text unchanged when
/// there are no rules.
pub fn process(rules: &[VoiceReplacement], text: String) -> String {
    if rules.is_empty() {
        return text;
    }
    Replacements::new(rules).apply(&text)
}

/// The whole final-transcript pipeline, in order: strip annotations and credits,
/// drop a vocabulary-prompt echo, then apply the replacement rules. The echo
/// check only applies to the embedded and HTTP kinds, the only ones that send
/// the vocabulary prompt.
pub fn finalize(cfg: &crate::config_voice::VoiceConfig, text: &str) -> String {
    use crate::config_voice::VoiceKind;
    let mut text = strip_non_speech(text, &cfg.language);
    if matches!(cfg.kind, VoiceKind::Embedded | VoiceKind::Http)
        && let Some(prompt) = cfg.vocabulary_prompt()
    {
        text = crate::voice::drop_prompt_echo(&text, &prompt);
    }
    process(&cfg.replacements, text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(from: &str, to: &str) -> VoiceReplacement {
        VoiceReplacement {
            from: from.into(),
            to: to.into(),
        }
    }

    fn run(rules: &[VoiceReplacement], text: &str) -> String {
        Replacements::new(rules).apply(text)
    }

    #[test]
    fn case_insensitive_with_punctuation_boundaries() {
        let rules = [rule("open code", "OpenCode")];
        assert_eq!(run(&rules, "OPEN CODE, open code."), "OpenCode, OpenCode.");
    }

    #[test]
    fn matches_only_on_word_boundaries() {
        let rules = [rule("open code", "OpenCode")];
        assert_eq!(
            run(&rules, "reopen code, open codes"),
            "reopen code, open codes"
        );
    }

    #[test]
    fn longest_rule_wins() {
        let rules = [
            rule("git hub", "GitHub"),
            rule("git hub actions", "GitHub Actions"),
        ];
        assert_eq!(run(&rules, "git hub actions"), "GitHub Actions");
    }

    #[test]
    fn earlier_rule_wins_ties() {
        let rules = [rule("cat", "dog"), rule("CAT", "lion")];
        assert_eq!(run(&rules, "cat"), "dog");
    }

    #[test]
    fn deletion_closes_the_gap() {
        let rules = [rule("um", "")];
        assert_eq!(run(&rules, "um so"), "so");
    }

    #[test]
    fn deletion_at_end_drops_preceding_space() {
        let rules = [rule("um", "")];
        assert_eq!(run(&rules, "well um."), "well.");
    }

    #[test]
    fn unicode_case_folding_and_boundaries() {
        assert_eq!(run(&[rule("über", "Über")], "über all"), "Über all");
        assert_eq!(
            run(&[rule("cafe au lait", "café au lait")], "cafe au lait"),
            "café au lait"
        );
    }

    #[test]
    fn phrase_inside_cjk_text_matches() {
        let rules = [rule("open code", "OpenCode")];
        assert_eq!(run(&rules, "请用open code编辑"), "请用OpenCode编辑");
    }

    #[test]
    fn finalize_strips_then_drops_echo_then_replaces() {
        use crate::config_voice::{VoiceConfig, VoiceKind};
        let mut cfg = VoiceConfig {
            kind: VoiceKind::Embedded,
            language: "en".into(),
            vocabulary: vec!["T3 Code".into(), "Ottobit".into()],
            replacements: vec![rule("open code", "OpenCode")],
            ..VoiceConfig::default()
        };
        // The echo is only visible once the annotation is stripped.
        assert_eq!(finalize(&cfg, "[BLANK_AUDIO] T3 Code, Ottobit"), "");
        assert_eq!(
            finalize(&cfg, "use open code [Music] Thanks for watching!"),
            "use OpenCode"
        );
        // Command providers never send the prompt, so an identical phrase stays.
        cfg.kind = VoiceKind::Command;
        assert_eq!(finalize(&cfg, "T3 Code, Ottobit"), "T3 Code, Ottobit");
    }

    #[test]
    fn empty_from_is_ignored_and_empty_rules_pass_through() {
        assert_eq!(run(&[rule("", "x")], "hello"), "hello");
        assert_eq!(process(&[], "hello".into()), "hello");
    }
}

#[cfg(test)]
mod strip_tests {
    use super::strip_non_speech;

    fn en(text: &str) -> String {
        strip_non_speech(text, "en")
    }

    #[test]
    fn removes_blank_audio_tag() {
        assert_eq!(en("[BLANK_AUDIO]"), "");
    }

    #[test]
    fn removes_bracket_annotation_mid_sentence() {
        assert_eq!(en("hello [Music] world"), "hello world");
    }

    #[test]
    fn removes_spacing_variant() {
        assert_eq!(en("[ Silence ]"), "");
    }

    #[test]
    fn keeps_parenthesized_speech() {
        assert_eq!(en("see (above)"), "see (above)");
    }

    #[test]
    fn removes_parenthesized_annotation() {
        assert_eq!(en("ok (laughter) fine"), "ok fine");
    }

    #[test]
    fn keeps_words_that_only_share_a_prefix_in_parentheses() {
        assert_eq!(en("(musical) notes"), "(musical) notes");
    }

    #[test]
    fn removes_trailing_credit() {
        assert_eq!(en("Ship it. Thanks for watching!"), "Ship it.");
    }

    #[test]
    fn keeps_credit_words_when_not_trailing() {
        assert_eq!(
            en("Thanks for watching the build"),
            "Thanks for watching the build"
        );
    }

    #[test]
    fn keeps_bare_thank_you() {
        assert_eq!(en("Thank you."), "Thank you.");
    }

    #[test]
    fn keeps_credit_glued_to_a_word() {
        assert_eq!(en("Nothanks for watching!"), "Nothanks for watching!");
    }

    #[test]
    fn foreign_credit_only_for_non_english() {
        let text = "Très bien. Спасибо за просмотр!";
        assert_eq!(strip_non_speech(text, "ru"), "Très bien.");
        assert_eq!(strip_non_speech(text, "en"), text);
    }

    #[test]
    fn removes_only_one_trailing_credit() {
        assert_eq!(
            en("Ship it. Subtitles by the Amara.org community"),
            "Ship it."
        );
    }

    #[test]
    fn unclosed_bracket_is_kept() {
        assert_eq!(en("[hello (music"), "[hello (music");
    }

    #[test]
    fn is_idempotent() {
        for text in [
            "[BLANK_AUDIO]",
            "hello [Music] world",
            "Ship it. Thanks for watching!",
            "see (above)",
            "Thank you.",
        ] {
            let once = en(text);
            assert_eq!(en(&once), once, "not idempotent for {text:?}");
        }
    }
}
