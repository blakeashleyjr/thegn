pub mod layout;
pub mod logs;
pub mod stat;
pub mod table;
pub mod timeseries;

/// Longest prefix of `s` holding at most `max_chars` chars (char-boundary safe).
///
/// Renderers clip every cell/line with this before handing it to a widget, so
/// layout cost is bounded by the panel width instead of the string length. The
/// budget is generous (see [`clip_budget`]) so the visible result is unchanged.
pub fn clip_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// Char budget for one cell/line in a panel `width` columns wide: 4x the width,
/// so zero-width/combining characters cannot starve the visible cells.
pub fn clip_budget(width: usize) -> usize {
    width.saturating_mul(4).max(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_is_char_boundary_safe() {
        assert_eq!(clip_chars("abcdef", 3), "abc");
        assert_eq!(clip_chars("日本語です", 2), "日本");
        assert_eq!(clip_chars("ab", 9), "ab");
        assert_eq!(clip_chars("", 0), "");
        assert_eq!(clip_budget(0), 4);
        assert_eq!(clip_budget(10), 40);
    }
}
