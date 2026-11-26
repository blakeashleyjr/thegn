//! Pure cleanup of Whisper output before it reaches a pane.
//!
//! Whisper emits annotations such as `[BLANK_AUDIO]` or `(music)` for
//! non-speech audio, and often ends a transcript with a subtitle credit that
//! was never spoken. [`strip_non_speech`] removes those. It runs before any
//! text replacement and never touches ordinary speech.

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

#[cfg(test)]
mod tests {
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
