//! Calendar display values are separate from provider values: filtering,
//! recurrence, URLs and semantic round-trip must keep their original text.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Each field has an independent scalar/cell limit. Input scanning also stops
/// after 4096 scalars, so a huge control/combining prefix cannot monopolize draw.
#[derive(Debug, Clone, Copy)]
pub enum Field {
    Title,
    Calendar,
    Location,
    Organizer,
    Category,
    Url,
    ExtensionLabel,
    ClockLabel,
    Reminder,
}

impl Field {
    pub const fn budget(self) -> (usize, usize) {
        match self {
            Self::Title | Self::Location | Self::Organizer => (256, 256),
            Self::Calendar | Self::Category | Self::ExtensionLabel | Self::ClockLabel => (64, 64),
            Self::Url => (256, 256),
            Self::Reminder => (512, 512),
        }
    }
}

/// Bidi overrides/isolation and invisible separators must not disguise field
/// boundaries. ZWJ/ZWNJ and variation selectors remain available for legitimate
/// script shaping/emoji. C0/C1 controls and all whitespace become separators.
fn separator(c: char) -> bool {
    c.is_control()
        || c.is_whitespace()
        || matches!(c,
        '\u{00ad}' | '\u{061c}' | '\u{180e}' | '\u{200b}' | '\u{200e}' | '\u{200f}'
        | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayText(String);

impl DisplayText {
    pub fn new(raw: &str, field: Field) -> Self {
        let (scalar_limit, cell_limit) = field.budget();
        let mut out = String::new();
        let mut scalars = 0;
        let mut cells = 0;
        let mut space = false;
        for c in raw.chars().take(4096) {
            if separator(c) {
                space = !out.is_empty();
                continue;
            }
            let width = UnicodeWidthChar::width(c).unwrap_or(0);
            // A leading combining character must not attach to an adjacent
            // field's last terminal cell. Keep marks after our own base text.
            if width == 0 && out.is_empty() {
                continue;
            }
            let padding = usize::from(space);
            if scalars + padding + 1 > scalar_limit || cells + padding + width > cell_limit {
                break;
            }
            if space {
                out.push(' ');
            }
            out.push(c);
            scalars += padding + 1;
            cells += padding + width;
            space = false;
        }
        Self(out)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn into_string(self) -> String {
        self.0
    }
    /// The same unicode-width model used by the host's cell measurements.
    pub fn cells(&self) -> usize {
        UnicodeWidthStr::width(self.0.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controls_and_invisible_boundaries_become_single_spaces() {
        let raw = "\r\n  alpha\t\x1b\x07\u{85}beta\u{202e}gamma\u{2066}delta\u{200b} \n";
        assert_eq!(
            DisplayText::new(raw, Field::Title).as_str(),
            "alpha beta gamma delta"
        );
        assert_eq!(raw.as_bytes()[0], b'\r'); // semantic input remains untouched
    }

    #[test]
    fn common_emoji_shaping_wide_text_and_combining_marks_survive() {
        let raw = "👩‍💻 ❤️ 中文 cafe\u{301} فارسی\u{200c}زبان";
        let text = DisplayText::new(raw, Field::Title);
        assert_eq!(text.as_str(), raw);
        assert_eq!(text.cells(), UnicodeWidthStr::width(raw));
        assert_eq!(
            DisplayText::new("\u{301}\u{200d}safe", Field::Title).as_str(),
            "safe"
        );
    }

    #[test]
    fn every_field_has_independent_scalar_cell_and_input_limits() {
        for field in [
            Field::Title,
            Field::Calendar,
            Field::Location,
            Field::Organizer,
            Field::Category,
            Field::Url,
            Field::ExtensionLabel,
            Field::ClockLabel,
            Field::Reminder,
        ] {
            for raw in [
                "界".repeat(10000),
                "a\u{301}".repeat(10000),
                "\n".repeat(10000),
                "x".repeat(10000),
            ] {
                let text = DisplayText::new(&raw, field);
                let (scalars, cells) = field.budget();
                assert!(text.as_str().chars().count() <= scalars);
                assert!(text.cells() <= cells);
                assert!(!text.as_str().chars().any(char::is_control));
                assert_eq!(text.as_str(), text.as_str().trim());
            }
        }
        assert!(
            DisplayText::new(&format!("{}hidden", "\n".repeat(4096)), Field::Title)
                .as_str()
                .is_empty()
        );
    }

    #[test]
    fn policy_is_idempotent() {
        let raw = "\n a\r\nb\t界\u{202e} 👩‍💻  ".repeat(400);
        let first = DisplayText::new(&raw, Field::Title);
        assert_eq!(DisplayText::new(first.as_str(), Field::Title), first);
    }
}
