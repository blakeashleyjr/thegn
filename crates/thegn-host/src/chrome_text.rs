//! Text sanitization for terminal chrome render boundaries.

/// Sanitize hostile text at the TUI's final render boundary (THE-507).
///
/// C0/C1 controls, DEL, and line separators become one visible replacement
/// character. Bidi formatting controls and invisible spacing/format controls
/// are removed. Joiners, variation selectors, tag characters, combining marks,
/// RTL letters, and ordinary text remain unchanged.
pub(crate) fn chrome_safe_text(text: &str) -> std::borrow::Cow<'_, str> {
    fn replacement(ch: char) -> Option<char> {
        let code = ch as u32;
        if ch.is_control() || matches!(code, 0x2028 | 0x2029) {
            Some('\u{FFFD}')
        } else {
            None
        }
    }

    fn is_removed(ch: char) -> bool {
        matches!(
            ch as u32,
            0x061c
                | 0x200e
                | 0x200f
                | 0x202a..=0x202e
                | 0x2066..=0x2069
                | 0x200b
                | 0x2060..=0x2064
                | 0xfeff
        )
    }

    fn needs_change(ch: char) -> bool {
        replacement(ch).is_some() || is_removed(ch)
    }

    let Some((first_index, first_char)) = text.char_indices().find(|(_, ch)| needs_change(*ch))
    else {
        return std::borrow::Cow::Borrowed(text);
    };

    let mut clean = String::with_capacity(text.len());
    clean.push_str(&text[..first_index]);
    for ch in std::iter::once(first_char).chain(text[first_index + first_char.len_utf8()..].chars())
    {
        if let Some(replacement) = replacement(ch) {
            clean.push(replacement);
        } else if !is_removed(ch) {
            clean.push(ch);
        }
    }
    std::borrow::Cow::Owned(clean)
}
