//! Lines-of-code report: the per-language breakdown behind the bottom-bar `LOC`
//! chip and its detail overlay. Produced off-loop by the host's `tokei` walk
//! (see `thegn-host/src/hydrate.rs`), serialized into the DB `loc_cache`, and
//! rendered as a tokei-style table (Language │ Files │ Lines │ Code │ Comments │
//! Blanks + a Total footer). This module owns the substrate-agnostic shape,
//! totalling, sorting, and the chip's compact-number formatting — no `tokei`
//! dep, so it stays testable in core.

use serde::{Deserialize, Serialize};

/// One language's counts — a single row of the tokei table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocLang {
    /// Display name (`tokei::LanguageType::name`), e.g. "Rust".
    pub name: String,
    /// Number of files attributed to the language.
    pub files: usize,
    /// Total lines = code + comments + blanks.
    pub lines: usize,
    pub code: usize,
    pub comments: usize,
    pub blanks: usize,
}

/// Whole-worktree LOC report: per-language rows (biggest first) plus column
/// totals. `total_code` is the number the bottom-bar chip shows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocReport {
    pub langs: Vec<LocLang>,
    pub total_files: usize,
    pub total_lines: usize,
    pub total_code: usize,
    pub total_comments: usize,
    pub total_blanks: usize,
}

impl LocReport {
    /// Build a report from per-language rows: sort by code descending (name as a
    /// stable tiebreak) so the biggest languages sit at the top of the table,
    /// then fold the column totals.
    pub fn from_langs(mut langs: Vec<LocLang>) -> LocReport {
        langs.sort_by(|a, b| b.code.cmp(&a.code).then_with(|| a.name.cmp(&b.name)));
        let mut r = LocReport {
            total_files: langs.iter().map(|l| l.files).sum(),
            total_lines: langs.iter().map(|l| l.lines).sum(),
            total_code: langs.iter().map(|l| l.code).sum(),
            total_comments: langs.iter().map(|l| l.comments).sum(),
            total_blanks: langs.iter().map(|l| l.blanks).sum(),
            langs,
        };
        // Guard against overflow surprises by keeping the derived total sane.
        r.total_lines = r.total_lines.max(r.total_code);
        r
    }

    /// A report carrying only a total (cache back-compat + tests): no rows.
    pub fn total_only(code: usize) -> LocReport {
        LocReport {
            total_code: code,
            total_lines: code,
            ..Default::default()
        }
    }

    /// The chip's compact number: "1.6M", "163.9k", or "42".
    pub fn compact_total(&self) -> String {
        let n = self.total_code as u64;
        if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            n.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lang(name: &str, code: usize) -> LocLang {
        LocLang {
            name: name.into(),
            files: 1,
            lines: code + 2,
            code,
            comments: 1,
            blanks: 1,
        }
    }

    #[test]
    fn from_langs_sorts_by_code_desc_and_sums() {
        let r = LocReport::from_langs(vec![lang("Ruby", 10), lang("Rust", 100), lang("Sh", 50)]);
        // Biggest language first.
        assert_eq!(
            r.langs.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
            vec!["Rust", "Sh", "Ruby"]
        );
        assert_eq!(r.total_code, 160);
        assert_eq!(r.total_files, 3);
        assert_eq!(r.total_comments, 3);
        assert_eq!(r.total_blanks, 3);
        // lines = (10+2)+(100+2)+(50+2) = 166
        assert_eq!(r.total_lines, 166);
    }

    #[test]
    fn equal_code_breaks_ties_by_name() {
        let r = LocReport::from_langs(vec![lang("Zig", 42), lang("Awk", 42)]);
        assert_eq!(
            r.langs.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(),
            vec!["Awk", "Zig"]
        );
    }

    #[test]
    fn empty_is_all_zero() {
        let r = LocReport::from_langs(vec![]);
        assert!(r.langs.is_empty());
        assert_eq!(
            (
                r.total_files,
                r.total_lines,
                r.total_code,
                r.total_comments,
                r.total_blanks
            ),
            (0, 0, 0, 0, 0)
        );
        assert_eq!(r.compact_total(), "0");
    }

    #[test]
    fn total_only_carries_the_chip_number() {
        let r = LocReport::total_only(1234);
        assert_eq!(r.total_code, 1234);
        assert!(r.langs.is_empty());
    }

    #[test]
    fn compact_total_boundaries() {
        assert_eq!(LocReport::total_only(999).compact_total(), "999");
        assert_eq!(LocReport::total_only(1_000).compact_total(), "1.0k");
        assert_eq!(LocReport::total_only(163_881).compact_total(), "163.9k");
        assert_eq!(LocReport::total_only(1_600_000).compact_total(), "1.6M");
    }

    #[test]
    fn serde_round_trip() {
        let r = LocReport::from_langs(vec![lang("Rust", 100), lang("Toml", 5)]);
        let json = serde_json::to_string(&r).unwrap();
        let back: LocReport = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
    }
}
