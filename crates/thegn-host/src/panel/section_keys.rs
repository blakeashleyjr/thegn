//! The panel's per-section key tables — one declaration of the action keys
//! each section handles in row mode, feeding the statusbar hint strip.
//!
//! Sibling of [`crate::sidebar_keytable`] and of
//! [`crate::panel::gitui::context_keys`] (which the git-family sections use,
//! and which additionally drives their dispatch).
//!
//! ## Why this exists
//!
//! These hints used to be inline literals in `panel/hints.rs`, written by hand
//! and never checked against the dispatch match in `run.rs`. They had drifted:
//!
//! - **Notifications** advertised `r` to mark a notification read; the actual
//!   key is `x`. Pressing `r` did nothing.
//! - **Issues** advertised `n` (new) and `e` (edit); neither is dispatched.
//! - **My work** advertised `b` (branch); not dispatched.
//! - Meanwhile plenty of live keys were never advertised at all (`Tests` has
//!   nine, of which four showed).
//!
//! `hint_table_matches_dispatch` (in the tests below) now reads the dispatch
//! match out of `run.rs` and fails if a table advertises a key the section does
//! not actually claim. It is a source-level check rather than a type-level one
//! — the per-section dispatch still lives in the `run.rs` loop, coupled to its
//! mutable state — but it turns silent drift into a failing test.
//!
//! Navigation keys (`j`/`k`/`↵`/`Esc`/`e`, the digit jumps) are handled by
//! `panel::accordion_key` for every section, so they are listed here with an
//! empty `key` and skipped by the dispatch check.

use crate::panel::Section;

/// One advertised section key.
pub struct SectionKey {
    /// The literal char the dispatch match claims. `None` for keys handled by
    /// the shared accordion map rather than a per-section arm.
    pub key: Option<char>,
    /// Display chord for the hint strip.
    pub chord: &'static str,
    pub label: &'static str,
}

const fn k(key: char, chord: &'static str, label: &'static str) -> SectionKey {
    SectionKey {
        key: Some(key),
        chord,
        label,
    }
}

/// A key the shared accordion map owns (not a per-section dispatch arm).
const fn nav(chord: &'static str, label: &'static str) -> SectionKey {
    SectionKey {
        key: None,
        chord,
        label,
    }
}

const MINE: &[SectionKey] = &[
    nav("j/k", "row"),
    nav("↵", "open"),
    k('o', "o", "browser"),
    k('a', "a", "all repos"),
    k('R', "R", "refresh"),
];
const PR: &[SectionKey] = &[
    nav("j/k", "row"),
    k('M', "M", "merge"),
    k('A', "A", "approve"),
    k('c', "c", "comment"),
    k('r', "r", "rerun"),
    k('o', "o", "browser"),
];
const TESTS: &[SectionKey] = &[
    k('r', "r", "run"),
    k('R', "R", "all"),
    k('f', "f", "failed"),
    k('o', "o", "output"),
    k('b', "b", "bisect"),
    nav("↵", "open"),
];
const CI: &[SectionKey] = &[
    nav("j/k", "row"),
    k('v', "v", "view"),
    k('r', "r", "rerun"),
    k('c', "c", "cancel"),
    k('g', "g", "refresh"),
    k('o', "o", "browser"),
];
const MERGE_QUEUE: &[SectionKey] = &[
    k('a', "a/A", "add"),
    k('x', "x", "remove"),
    k('l', "l", "land"),
    k('r', "r", "retry"),
    k('D', "D", "drain"),
];
const FILES: &[SectionKey] = &[
    nav("↵", "open"),
    k('o', "o", "editor"),
    k('b', "b", "blame"),
    k('y', "y", "yazi"),
];
const ISSUES: &[SectionKey] = &[
    nav("j/k", "row"),
    nav("↵", "link"),
    k('o', "o", "browser"),
    k('a', "a", "assign me"),
    k('D', "D", "dispatch"),
    k('r', "r", "refresh"),
];
const NOTIFICATIONS: &[SectionKey] = &[
    nav("j/k", "row"),
    nav("↵", "expand"),
    // `x`, not `r` — the old hint advertised a key that did nothing.
    k('x', "x", "read"),
    k('d', "d", "dismiss"),
    k('A', "A", "show all"),
    k('/', "/", "search"),
];
const JOBS: &[SectionKey] = &[
    nav("↵", "run"),
    k('r', "r", "re-run"),
    k('s', "s", "stop"),
    k('o', "o", "output"),
    nav("j/k", "select"),
];
const LOGS: &[SectionKey] = &[
    nav("j/k", "row"),
    k('/', "/", "filter"),
    k('l', "l", "level"),
    k('y', "y", "copy"),
    k('a', "a", "all scopes"),
    k('e', "e", "export"),
];
const SYMBOLS: &[SectionKey] = &[
    nav("↵", "go to def"),
    k('r', "r", "refs"),
    k('h', "h", "hover"),
    k('o', "o", "outline"),
    nav("j/k", "select"),
];
const PROBLEMS: &[SectionKey] = &[nav("↵", "open"), nav("j/k", "select")];
const FORWARD: &[SectionKey] = &[
    nav("j/k", "row"),
    k('o', "o", "open in browser"),
    nav("↵", "copy url"),
];
const HOSTS: &[SectionKey] = &[
    nav("j/k", "row"),
    k('n', "n", "new"),
    k('r', "r", "refresh"),
    k('m', "m", "menu"),
    k('x', "x", "remove"),
];
const ENVIRONMENTS: &[SectionKey] = &[
    nav("j/k", "row"),
    k('n', "n", "new"),
    k('t', "t", "test"),
    k('x', "x", "remove"),
];
const MEDIA: &[SectionKey] = &[
    nav("space", "play/pause"),
    nav("n/p", "next/prev"),
    nav("s", "shuffle"),
    nav("L", "loop"),
];
const SHARE: &[SectionKey] = &[nav("j/k", "row"), nav("↵", "copy url")];
const ROW_ONLY: &[SectionKey] = &[nav("j/k", "row")];

/// Row-mode keys for a section. Order is display order; the statusbar shows a
/// prefix, so the most useful keys come first.
pub fn section_keys(section: Section) -> &'static [SectionKey] {
    match section {
        Section::Mine => MINE,
        Section::Pr => PR,
        Section::Tests => TESTS,
        Section::Ci => CI,
        Section::MergeQueue => MERGE_QUEUE,
        Section::Files => FILES,
        Section::Issues => ISSUES,
        Section::Notifications => NOTIFICATIONS,
        Section::Jobs => JOBS,
        Section::Logs => LOGS,
        Section::Symbols => SYMBOLS,
        Section::Problems => PROBLEMS,
        Section::Forward => FORWARD,
        Section::Hosts => HOSTS,
        Section::Environments => ENVIRONMENTS,
        Section::Media => MEDIA,
        Section::Share => SHARE,
        // Row-nav-only sections (Debug, Db, Telemetry, Keys, Across, Help, …)
        // and the git family, which draws from `gitui::context_keys` instead.
        _ => ROW_ONLY,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panel::SECTION_ORDER;
    use std::collections::{BTreeMap, BTreeSet};

    /// Parse the per-section dispatch match out of `run.rs`, returning the set
    /// of chars each section actually claims.
    ///
    /// Deliberately source-level: the arms are ordinary `match` patterns bound
    /// to the event loop's mutable state, so there is no runtime table to read.
    /// The patterns are uniform enough (`(Section::X, KeyCode::Char('c'))`,
    /// char-alternation, and a couple of `matches!` guards) to scan reliably.
    fn dispatched() -> BTreeMap<String, BTreeSet<char>> {
        let src = include_str!("../run.rs");
        let anchor = src
            .find("let handled = match (panel_ui.open, k.key)")
            .expect("the per-section dispatch match must exist in run.rs");
        // Walk to the end of the match by brace depth.
        let bytes: Vec<char> = src[anchor..].chars().collect();
        let mut depth = 0i32;
        let mut stop = bytes.len();
        let mut started = false;
        for (i, c) in bytes.iter().enumerate() {
            match c {
                '{' => {
                    depth += 1;
                    started = true;
                }
                '}' => {
                    depth -= 1;
                    if started && depth == 0 {
                        stop = i;
                        break;
                    }
                }
                _ => {}
            }
        }
        let region: String = bytes[..stop].iter().collect();

        let mut out: BTreeMap<String, BTreeSet<char>> = BTreeMap::new();
        let chars: Vec<char> = region.chars().collect();
        for (i, c) in chars.iter().enumerate() {
            if *c != '(' {
                continue;
            }
            // Only an arm pattern, never a call like `PanelHit::Row(Section::X,
            // ..)`: the `(` must not follow an identifier.
            if i > 0
                && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_' || chars[i - 1] == '!')
            {
                continue;
            }
            // Skip whitespace, then require `Section::`.
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            let tail: String = chars[j..(j + 9).min(chars.len())].iter().collect();
            if tail != "Section::" {
                continue;
            }
            j += 9;
            let name: String = chars[j..]
                .iter()
                .take_while(|c| c.is_alphanumeric())
                .collect();
            if name.is_empty() {
                continue;
            }
            // The pattern runs up to the arm body.
            let rest: String = chars[j..].iter().collect();
            let pattern = rest.split("=>").next().unwrap_or(&rest);
            let entry = out.entry(name).or_default();
            let pc: Vec<char> = pattern.chars().collect();
            let mut m = 0;
            while m + 2 < pc.len() {
                if pc[m] == '\'' && pc[m + 2] == '\'' {
                    entry.insert(pc[m + 1]);
                    m += 3;
                } else {
                    m += 1;
                }
            }
        }
        out
    }

    /// The gate: a section may not advertise a key it does not dispatch.
    ///
    /// This is the check that was missing — the Notifications strip told users
    /// to press `r` to mark a notification read for as long as the section has
    /// existed, while the dispatch has always used `x`.
    #[test]
    fn hint_table_matches_dispatch() {
        let dispatched = dispatched();
        assert!(
            dispatched.len() > 5,
            "the dispatch scan found almost nothing — it has probably broken: {:?}",
            dispatched.keys().collect::<Vec<_>>()
        );

        let mut dead: Vec<String> = Vec::new();
        for section in SECTION_ORDER {
            // The git family dispatches through `gitui`, not this match.
            if section.is_git_family() {
                continue;
            }
            let name = format!("{section:?}");
            let Some(live) = dispatched.get(&name) else {
                // No per-section arms at all: only nav keys may be advertised.
                for sk in section_keys(section) {
                    if let Some(c) = sk.key {
                        dead.push(format!(
                            "{name}: `{c}` ({}) — section has no dispatch arms",
                            sk.label
                        ));
                    }
                }
                continue;
            };
            for sk in section_keys(section) {
                if let Some(c) = sk.key
                    && !live.contains(&c)
                {
                    dead.push(format!(
                        "{name}: `{c}` ({}) is advertised but not dispatched",
                        sk.label
                    ));
                }
            }
        }
        assert!(
            dead.is_empty(),
            "hint tables advertise keys that do nothing:\n  {}\n\
             Either wire the key up in run.rs's per-section match, or drop the row.",
            dead.join("\n  ")
        );
    }

    /// Every section resolves to a table (the catch-all keeps this total), and
    /// no table repeats a chord.
    #[test]
    fn tables_are_total_and_unambiguous() {
        for section in SECTION_ORDER {
            let keys = section_keys(section);
            assert!(!keys.is_empty(), "{section:?} has no hint row");
            let mut seen = BTreeSet::new();
            for sk in keys {
                assert!(
                    seen.insert(sk.chord),
                    "{section:?} lists `{}` twice",
                    sk.chord
                );
                assert!(
                    !sk.label.is_empty(),
                    "{section:?}: `{}` has no label",
                    sk.chord
                );
            }
        }
    }

    /// The strip is one line — keep every section skimmable.
    #[test]
    fn tables_stay_short() {
        for section in SECTION_ORDER {
            let n = section_keys(section).len();
            assert!(n <= 6, "{section:?} advertises {n} keys; the strip fits ~6");
        }
    }
}
