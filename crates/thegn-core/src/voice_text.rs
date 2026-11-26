//! Replacement rules applied to finished dictation text.
//!
//! Pure and I/O-free. Rules are case-insensitive literal phrases matched only
//! on word boundaries. Overlapping matches are resolved longest-first, then by
//! earlier start, then by earlier rule. This is deliberately simpler than Hex:
//! no re-capitalization and no punctuation clean-up beyond one adjacent space.

use regex::Regex;

use crate::config_voice::{MAX_REPLACEMENTS, VoiceReplacement};

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
    fn empty_from_is_ignored_and_empty_rules_pass_through() {
        assert_eq!(run(&[rule("", "x")], "hello"), "hello");
        assert_eq!(process(&[], "hello".into()), "hello");
    }
}
