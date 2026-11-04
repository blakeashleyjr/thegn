//! Grapheme-safe, bounded text editing, independent of a terminal or a clock.
use std::collections::VecDeque;
use tg_kit::{Key, Modifiers};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const FIELD_BYTES: usize = 32 * 1024;
pub const UNDO_TRANSACTIONS: usize = 64;

#[derive(Debug, Clone)]
struct Edit {
    start: usize,
    removed: String,
    inserted: String,
    before: (usize, Option<usize>),
    after: (usize, Option<usize>),
}

/// Cursor and anchor are UTF-8 byte positions at extended grapheme boundaries.
#[derive(Debug, Clone)]
pub struct Editor {
    text: String,
    cursor: usize,
    anchor: Option<usize>,
    undo: VecDeque<Edit>,
    redo: Vec<Edit>,
    pub scroll_line: usize,
    pub scroll_column: usize,
    preferred_column: Option<usize>,
}
impl Editor {
    pub fn new(text: String) -> Self {
        let cursor = text.len();
        Self {
            text,
            cursor,
            anchor: None,
            undo: VecDeque::new(),
            redo: Vec::new(),
            scroll_line: 0,
            scroll_column: 0,
            preferred_column: None,
        }
    }
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn cursor(&self) -> usize {
        self.cursor
    }
    pub fn selection(&self) -> Option<std::ops::Range<usize>> {
        self.anchor
            .filter(|a| *a != self.cursor)
            .map(|a| a.min(self.cursor)..a.max(self.cursor))
    }
    pub fn history_bytes(&self) -> usize {
        self.undo
            .iter()
            .chain(self.redo.iter())
            .map(|e| e.inserted.len() + e.removed.len())
            .sum()
    }
    pub fn drop_oldest_history(&mut self) -> bool {
        if self.undo.pop_front().is_some() {
            true
        } else {
            self.redo.pop().is_some()
        }
    }
    pub fn replace_external(&mut self, text: String) {
        *self = Self::new(text);
    }
    fn preceding(&self, at: usize) -> usize {
        self.text[..at]
            .grapheme_indices(true)
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }
    fn following(&self, at: usize) -> usize {
        self.text[at..]
            .graphemes(true)
            .next()
            .map_or(at, |g| at + g.len())
    }
    fn word_left(&self) -> usize {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(char::is_whitespace);
        trimmed
            .split_word_bound_indices()
            .rev()
            .find(|(_, s)| s.chars().any(char::is_alphanumeric))
            .map(|(i, _)| i)
            .unwrap_or(0)
    }
    fn word_right(&self) -> usize {
        let after = &self.text[self.cursor..];
        let mut iter = after.split_word_bound_indices();
        let mut end = self.cursor;
        for (i, s) in iter.by_ref() {
            end = self.cursor + i + s.len();
            if s.chars().any(char::is_alphanumeric) {
                break;
            }
        }
        end
    }
    fn move_cursor(&mut self, to: usize, selecting: bool) {
        if selecting {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = to;
    }
    /// Reject an oversized replacement before changing text or its selection.
    pub fn can_insert(&self, text: &str) -> bool {
        self.text.len() - self.selection().map_or(0, |r| r.len()) + text.len() <= FIELD_BYTES
    }
    pub fn insert(&mut self, inserted: &str) -> bool {
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        if self.text.len() - range.len() + inserted.len() > FIELD_BYTES {
            return false;
        }
        let before = (self.cursor, self.anchor);
        let removed = self.text[range.clone()].to_string();
        if removed == inserted {
            return false;
        }
        self.text.replace_range(range.clone(), inserted);
        // An inserted combining sequence can merge with its preceding grapheme.
        let raw_cursor = range.start + inserted.len();
        self.cursor = self
            .text
            .grapheme_indices(true)
            .map(|(i, g)| i + g.len())
            .find(|i| *i >= raw_cursor)
            .unwrap_or(self.text.len());
        self.anchor = None;
        self.preferred_column = None;
        self.undo.push_back(Edit {
            start: range.start,
            removed,
            inserted: inserted.into(),
            before,
            after: (self.cursor, None),
        });
        while self.undo.len() > UNDO_TRANSACTIONS {
            self.undo.pop_front();
        }
        self.redo.clear();
        true
    }
    pub fn replace_all(&mut self, text: &str) -> bool {
        let before = (self.cursor, self.anchor);
        self.anchor = Some(0);
        self.cursor = self.text.len();
        let changed = self.insert(text);
        if changed {
            if let Some(edit) = self.undo.back_mut() {
                edit.before = before;
            }
        } else {
            (self.cursor, self.anchor) = before;
        }
        changed
    }
    pub fn undo(&mut self) -> bool {
        let Some(edit) = self.undo.pop_back() else {
            return false;
        };
        self.text
            .replace_range(edit.start..edit.start + edit.inserted.len(), &edit.removed);
        (self.cursor, self.anchor) = edit.before;
        self.redo.push(edit);
        true
    }
    pub fn redo(&mut self) -> bool {
        let Some(edit) = self.redo.pop() else {
            return false;
        };
        self.text
            .replace_range(edit.start..edit.start + edit.removed.len(), &edit.inserted);
        (self.cursor, self.anchor) = edit.after;
        self.undo.push_back(edit);
        true
    }
    fn vertical(&mut self, down: bool, selecting: bool) {
        let start = self.text[..self.cursor].rfind('\n').map_or(0, |p| p + 1);
        let col = *self
            .preferred_column
            .get_or_insert_with(|| display_width(&self.text[start..self.cursor]));
        let target = if down {
            let Some(next) = self.text[self.cursor..]
                .find('\n')
                .map(|p| self.cursor + p + 1)
            else {
                return;
            };
            next
        } else {
            if start == 0 {
                return;
            }
            self.text[..start - 1].rfind('\n').map_or(0, |p| p + 1)
        };
        let line_end = self.text[target..]
            .find('\n')
            .map_or(self.text.len(), |p| target + p);
        let mut offset = target;
        let mut width = 0;
        for (i, g) in self.text[target..line_end].grapheme_indices(true) {
            let w = if g == "\t" {
                4 - width % 4
            } else {
                UnicodeWidthStr::width(g)
            };
            if width + w > col {
                break;
            }
            width += w;
            offset = target + i + g.len();
        }
        self.move_cursor(offset, selecting);
    }
    /// Return whether visible editor state changed. Read-only handling belongs to the UI.
    pub fn key(&mut self, key: Key, mods: Modifiers, multiline: bool) -> bool {
        let key = match key {
            Key::Char(c) if mods.ctrl => Key::Char(c.to_ascii_lowercase()),
            other => other,
        };
        let before = (self.text.clone(), self.cursor, self.anchor);
        match key {
            Key::Char('a') if mods.ctrl => {
                self.anchor = Some(0);
                self.cursor = self.text.len();
            }
            Key::Char('z') if mods.ctrl && mods.shift => {
                self.redo();
            }
            Key::Char('z') if mods.ctrl => {
                self.undo();
            }
            Key::Char('y') if mods.ctrl => {
                self.redo();
            }
            Key::Char(c) if !mods.ctrl && !mods.alt && !c.is_control() => {
                self.insert(&c.to_string());
            }
            Key::Enter if multiline && !mods.ctrl => {
                self.insert("\n");
            }
            Key::Left => {
                let to = if !mods.shift {
                    self.selection().map(|r| r.start)
                } else {
                    None
                }
                .unwrap_or_else(|| {
                    if mods.ctrl || mods.alt {
                        self.word_left()
                    } else {
                        self.preceding(self.cursor)
                    }
                });
                self.move_cursor(to, mods.shift);
                self.preferred_column = None;
            }
            Key::Right => {
                let to = if !mods.shift {
                    self.selection().map(|r| r.end)
                } else {
                    None
                }
                .unwrap_or_else(|| {
                    if mods.ctrl || mods.alt {
                        self.word_right()
                    } else {
                        self.following(self.cursor)
                    }
                });
                self.move_cursor(to, mods.shift);
                self.preferred_column = None;
            }
            Key::Home => {
                let to = if mods.ctrl {
                    0
                } else {
                    self.text[..self.cursor].rfind('\n').map_or(0, |p| p + 1)
                };
                self.move_cursor(to, mods.shift);
                self.preferred_column = None;
            }
            Key::End => {
                let to = if mods.ctrl {
                    self.text.len()
                } else {
                    self.text[self.cursor..]
                        .find('\n')
                        .map_or(self.text.len(), |p| self.cursor + p)
                };
                self.move_cursor(to, mods.shift);
                self.preferred_column = None;
            }
            Key::Up if multiline => self.vertical(false, mods.shift),
            Key::Down if multiline => self.vertical(true, mods.shift),
            Key::Backspace => {
                if self.selection().is_none() {
                    self.anchor = Some(if mods.ctrl {
                        self.word_left()
                    } else {
                        self.preceding(self.cursor)
                    });
                }
                if self.insert("") {
                    if let Some(edit) = self.undo.back_mut() {
                        edit.before = (before.1, before.2);
                    }
                } else {
                    self.anchor = before.2;
                }
            }
            Key::Delete => {
                if self.selection().is_none() {
                    self.anchor = Some(if mods.ctrl {
                        self.word_right()
                    } else {
                        self.following(self.cursor)
                    });
                }
                if self.insert("") {
                    if let Some(edit) = self.undo.back_mut() {
                        edit.before = (before.1, before.2);
                    }
                } else {
                    self.anchor = before.2;
                }
            }
            _ => {}
        }
        before != (self.text.clone(), self.cursor, self.anchor)
    }
    pub fn line_column(&self) -> (usize, usize) {
        let before = &self.text[..self.cursor];
        let line = before.bytes().filter(|b| *b == b'\n').count();
        let col = display_width(before.rsplit('\n').next().unwrap_or(""));
        (line, col)
    }
    pub fn follow_caret(&mut self, width: usize, height: usize) {
        // Preparing a hidden or not-yet-sized widget must preserve its visible
        // editing context until the host supplies a real viewport.
        if width == 0 || height == 0 {
            return;
        }
        let (line, col) = self.line_column();
        if line < self.scroll_line {
            self.scroll_line = line;
        }
        if line >= self.scroll_line.saturating_add(height) {
            self.scroll_line = line + 1 - height;
        }
        self.scroll_line = self
            .scroll_line
            .min(self.text.split('\n').count().saturating_sub(height));
        if col < self.scroll_column {
            self.scroll_column = col;
        }
        if col >= self.scroll_column.saturating_add(width) {
            self.scroll_column = col + 1 - width;
        }
        // Expanding a viewport reveals preceding content when the whole line
        // now fits. Reserve one column for a caret at the end of the line.
        let line_width = self.text.split('\n').nth(line).map_or(0, display_width);
        self.scroll_column = self
            .scroll_column
            .min(line_width.max(col).saturating_add(1).saturating_sub(width));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_size_preserves_scroll_and_expanding_viewport_reveals_context() {
        let mut editor = Editor::new("baseline".into());
        editor.follow_caret(0, 0);
        assert_eq!((editor.scroll_column, editor.scroll_line), (0, 0));
        editor.follow_caret(3, 1);
        assert_eq!(editor.scroll_column, 6);
        editor.follow_caret(0, 1);
        editor.follow_caret(20, 0);
        assert_eq!(editor.scroll_column, 6);
        editor.follow_caret(20, 1);
        assert_eq!(editor.scroll_column, 0);

        let mut editor = Editor::new("first\nsecond\nthird".into());
        editor.follow_caret(3, 1);
        assert_eq!((editor.scroll_column, editor.scroll_line), (3, 2));
        editor.follow_caret(20, 3);
        assert_eq!((editor.scroll_column, editor.scroll_line), (0, 0));
    }
    #[test]
    fn deletion_and_selection_preserve_extended_graphemes() {
        let mut e = Editor::new("A👩‍💻e\u{301}界".into());
        e.key(Key::Left, Modifiers::NONE, false);
        e.key(Key::Backspace, Modifiers::NONE, false);
        assert_eq!(e.text(), "A👩‍💻界");
        e.key(
            Key::Left,
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
            false,
        );
        assert_eq!(
            e.selection().map(|r| e.text()[r].to_string()),
            Some("👩‍💻".into())
        );
        assert!(e.insert("B"));
        assert_eq!(e.text(), "AB界");
        assert!(e.undo());
        assert_eq!(e.text(), "A👩‍💻界");
        assert!(e.redo());
        assert_eq!(e.text(), "AB界");
    }
    #[test]
    fn rejected_paste_preserves_text_selection_and_history() {
        let mut e = Editor::new("abc".into());
        e.key(Key::Home, Modifiers::NONE, false);
        assert!(!e.insert(&"z".repeat(FIELD_BYTES + 1)));
        assert_eq!(e.text(), "abc");
        assert_eq!(e.cursor(), 0);
        assert_eq!(e.history_bytes(), 0);
    }
    #[test]
    fn vertical_movement_uses_terminal_columns_and_scroll_follows() {
        let mut e = Editor::new("界x\nabc\nlast".into());
        e.key(
            Key::Home,
            Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            },
            true,
        );
        e.key(Key::Right, Modifiers::NONE, true);
        e.key(Key::Down, Modifiers::NONE, true);
        assert_eq!(&e.text()[..e.cursor()], "界x\nab");
        e.follow_caret(2, 1);
        assert_eq!((e.scroll_line, e.scroll_column), (1, 1));
    }
    #[test]
    fn bounded_history_keeps_latest_transactions() {
        let mut e = Editor::new(String::new());
        for _ in 0..100 {
            e.insert("a");
        }
        for _ in 0..100 {
            e.undo();
        }
        assert_eq!(e.text().len(), 36);
    }
}

#[cfg(test)]
mod joined_grapheme_test {
    use super::*;
    #[test]
    fn inserting_joiner_snaps_to_the_new_whole_grapheme_and_undo_is_safe() {
        let mut editor = Editor::new("👩💻".into());
        editor.key(Key::Left, Modifiers::NONE, false);
        assert!(editor.insert("\u{200d}"));
        assert_eq!(editor.text(), "👩‍💻");
        assert_eq!(editor.cursor(), editor.text().len());
        assert!(editor.undo());
        assert_eq!(editor.text(), "👩💻");
        assert_eq!(editor.cursor(), "👩".len());
        assert!(editor.redo());
        editor.key(
            Key::Left,
            Modifiers {
                shift: true,
                ..Modifiers::NONE
            },
            false,
        );
        assert_eq!(editor.selection(), Some(0..editor.text().len()));
        assert!(editor.insert("X"));
        assert_eq!(editor.text(), "X");
    }
}

pub(crate) fn display_width(text: &str) -> usize {
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        column += if grapheme == "\t" {
            4 - column % 4
        } else {
            UnicodeWidthStr::width(grapheme)
        };
    }
    column
}

#[cfg(test)]
mod undo_caret_tests {
    use super::*;
    #[test]
    fn undo_deletion_restores_original_caret_without_a_synthetic_selection() {
        let mut editor = Editor::new("a界".into());
        editor.key(Key::Backspace, Modifiers::NONE, false);
        assert_eq!(editor.text(), "a");
        assert!(editor.undo());
        assert_eq!(editor.text(), "a界");
        assert_eq!(editor.cursor(), editor.text().len());
        assert!(editor.selection().is_none());
        assert!(editor.redo());
        assert_eq!(editor.text(), "a");
    }
}
