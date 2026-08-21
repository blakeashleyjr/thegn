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
    k('b', "b", "branch"),
    k('o', "o", "browser"),
    k('a', "a", "all repos"),
    k('R', "R", "refresh"),
];
const ACROSS: &[SectionKey] = &[
    nav("j/k", "row"),
    nav("↵", "jump"),
    k('a', "a", "all workspaces"),
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
    k('g', "g", "scope"),
];
// Labels match what the keys actually do (`o` opens bat, the EDITOR is `O`;
// the old strip claimed `o` = editor and `b` = blame, neither true here —
// blame is a git-family action the Files tree never dispatches).
const FILES: &[SectionKey] = &[
    nav("↵", "preview"),
    k('/', "/", "filter"),
    k('o', "o", "bat"),
    k('O', "O", "editor"),
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
    // `x`, not `r` — the old hint advertised a key that did nothing. Labels
    // match the section body's own hint row: `x` marks read, `d` DELETES.
    k('x', "x", "read"),
    k('d', "d", "delete"),
    // `a` and `g` are handled here but were never advertised. `a` is the only
    // gesture that also quiets the *live* needs-you signals (failing CI &c.,
    // which are derived from the PR/CI cache rather than an inbox row), so
    // leaving it hidden made the `✋` badge look unclearable; `g` is the toggle
    // between this repo and every worktree, so without it the default scoping is
    // invisible. The strip fits ~6, so they displace `A` (show-read) and `/`
    // (search) — both still work, and both are conventions carried by other
    // sections' strips.
    k('a', "a", "clear all"),
    k('g', "g", "scope"),
];
const JOBS: &[SectionKey] = &[
    nav("↵", "run"),
    k('r', "r", "re-run"),
    k('s', "s", "stop"),
    k('o', "o", "output"),
    nav("j/k", "select"),
];
const LOGS: &[SectionKey] = &[
    k('/', "/", "filter"),
    k('l', "l", "level"),
    k('y', "y", "copy"),
    // `a` toggles tail-follow; the scope toggle is `g` (the old strip said
    // `a` = "all scopes", which was a different key's job).
    k('a', "a", "tail"),
    k('g', "g", "scope"),
    k('E', "E", "export"),
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
    k('n', "n", "new"),
    k('p', "p", "provision"),
    k('r', "r", "probe"),
    k('m', "m", "menu"),
    k('c', "c", "grant"),
    // `x` forgets the cached host state (confirmed) — actual host REMOVAL
    // lives behind `m`; labelling this "remove" made users expect deletion.
    k('x', "x", "forget cache"),
];
const ENVIRONMENTS: &[SectionKey] = &[
    nav("j/k", "row"),
    nav("↵", "bind here"),
    k('n', "n", "new"),
    k('t', "t", "test"),
    k('x', "x", "remove"),
];
const MEDIA: &[SectionKey] = &[
    k(' ', "space", "play/pause"),
    k('n', "n", "next"),
    k('p', "p", "prev"),
    k('s', "s", "shuffle"),
    k('L', "L", "loop"),
    nav("↵", "panel"),
];
const SANDBOX: &[SectionKey] = &[
    k('s', "s", "stop"),
    k('r', "r", "restart"),
    k('l', "l", "logs"),
    k('g', "g", "scope"),
];
const SHARE: &[SectionKey] = &[
    nav("j/k", "row"),
    nav("↵", "copy url"),
    k('o', "o", "browser"),
    k('x', "x", "stop"),
];
const ROW_ONLY: &[SectionKey] = &[nav("j/k", "row")];

/// Row-mode keys for a section. Order is display order; the statusbar shows a
/// prefix, so the most useful keys come first.
pub fn section_keys(section: Section) -> &'static [SectionKey] {
    match section {
        Section::Mine => MINE,
        Section::Across => ACROSS,
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
        Section::Sandbox => SANDBOX,
        Section::Hosts => HOSTS,
        Section::Environments => ENVIRONMENTS,
        Section::Media => MEDIA,
        Section::Share => SHARE,
        // Row-nav-only sections (Debug, Db, Telemetry, Keys, Help, …)
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
            // Collect the chars once…
            let pc: Vec<char> = pattern.chars().collect();
            let mut arm_chars: Vec<char> = Vec::new();
            let mut m = 0;
            while m + 2 < pc.len() {
                if pc[m] == '\'' && pc[m + 2] == '\'' {
                    arm_chars.push(pc[m + 1]);
                    m += 3;
                } else {
                    m += 1;
                }
            }
            // …and credit EVERY section in the arm's pattern, not just the
            // first: `(Section::Notifications | Section::Sandbox, Char('g'))`
            // dispatches `g` for both, and crediting only the first made a
            // truthful Sandbox table fail the drift check.
            let mut names = vec![name];
            let mut rest_pat = pattern;
            while let Some(pos) = rest_pat.find("Section::") {
                let after = &rest_pat[pos + 9..];
                let extra: String = after.chars().take_while(|c| c.is_alphanumeric()).collect();
                if !extra.is_empty() && !names.contains(&extra) {
                    names.push(extra);
                }
                rest_pat = &rest_pat[pos + 9..];
            }
            for n in names {
                let entry = out.entry(n).or_default();
                entry.extend(arm_chars.iter().copied());
            }
        }
        out
    }

    /// The gate: a section may not advertise a key it does not dispatch.
    ///
    /// This is the check that was missing — the Notifications strip told users
    /// to press `r` to mark a notification read for as long as the section has
    /// existed, while the dispatch has always used `x`.
    /// A chord that is one ASCII letter (or `space`) can only be a per-section
    /// key; the shared accordion vocabulary is arrows, `j/k`, `↵`, `1-9`, etc.
    fn looks_like_section_key(chord: &str) -> bool {
        chord == "space" || (chord.len() == 1 && chord.chars().all(|c| c.is_ascii_alphabetic()))
    }

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
                    } else if looks_like_section_key(sk.chord) {
                        dead.push(format!(
                            "{name}: `{}` ({}) is nav() but looks like a section key",
                            sk.chord, sk.label
                        ));
                    }
                }
                continue;
            };
            for sk in section_keys(section) {
                // A `nav()` row with a single-letter chord is a section key in
                // disguise — declared as shared-accordion so this test would
                // skip it (the Media table shipped four such rows).
                if sk.key.is_none() && looks_like_section_key(sk.chord) {
                    dead.push(format!(
                        "{name}: `{}` ({}) is declared nav() but is a single-key chord — use k()",
                        sk.chord, sk.label
                    ));
                }
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

    /// Every `nav()` chord must be one the shared accordion map actually
    /// handles — `nav` entries skip the dispatch check above, so a table
    /// could otherwise advertise a navigation key nothing implements (the
    /// dead MEDIA-table class of drift).
    #[test]
    fn nav_chords_are_real_accordion_keys() {
        // The accordion's own vocabulary (`panel::accordion_key` + the shared
        // row-mode arms): cursor moves, open/close, width cycle, digit jumps,
        // tab cycle, and Changes' space.
        const ACCORDION: &[&str] = &[
            "j/k", "↑/↓", "↵", "esc", "e", "E", "⇥", "space", "J/K", "digits", "1-9",
        ];
        for section in SECTION_ORDER {
            for sk in section_keys(section) {
                if sk.key.is_none() {
                    assert!(
                        ACCORDION.contains(&sk.chord),
                        "{section:?}: nav chord `{}` is not an accordion key — \
                         either dispatch it (use `k(..)`) or drop the hint",
                        sk.chord
                    );
                }
            }
        }
    }

    /// Best-effort cross-surface label agreement: when a single-char chord in
    /// a section's statusbar table also appears in that section's in-body
    /// `hint_row`, the two labels must agree (the `x` = "read" vs "dismiss"
    /// class of drift). Source-level scan over the single-section renderer
    /// files; multi-section files (misc.rs) and chord aliases are skipped.
    #[test]
    fn statusbar_and_hint_row_labels_agree() {
        let files: &[(Section, &str)] = &[
            (
                Section::Notifications,
                include_str!("sections/notifications.rs"),
            ),
            (Section::Logs, include_str!("sections/logs.rs")),
            (Section::MergeQueue, include_str!("sections/merge_queue.rs")),
            (Section::Across, include_str!("sections/across.rs")),
            (Section::Ci, include_str!("sections/ci.rs")),
            (Section::Hosts, include_str!("sections/hosts.rs")),
            (Section::Issues, include_str!("sections/issues.rs")),
            (Section::Jobs, include_str!("sections/tasks.rs")),
            (Section::Problems, include_str!("sections/problems.rs")),
            (Section::Symbols, include_str!("sections/symbols.rs")),
        ];
        for (section, src) in files {
            // Collect ("chord", "label") pairs from every hint_row(&[...])
            // call in the file: successive `("a", "b")` string-literal pairs.
            let mut body_labels: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            let mut rest = *src;
            while let Some(pos) = rest.find("hint_row(&[") {
                rest = &rest[pos + "hint_row(&[".len()..];
                let end = rest.find("])").unwrap_or(rest.len());
                let block = &rest[..end];
                // Walk the block's top-level `(...)` tuples by paren depth
                // (labels may be `if`-expressions spanning lines and calls);
                // within a tuple, the FIRST string literal is the chord and
                // every remaining literal is one of its possible labels.
                let mut depth = 0usize;
                let mut tuple = String::new();
                for c in block.chars() {
                    match c {
                        '(' => {
                            depth += 1;
                            if depth == 1 {
                                tuple.clear();
                                continue;
                            }
                        }
                        ')' => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                let lits: Vec<&str> = tuple
                                    .split('"')
                                    .enumerate()
                                    .filter(|(i, _)| i % 2 == 1)
                                    .map(|(_, s)| s)
                                    .collect();
                                if let Some((chord, labels)) = lits.split_first() {
                                    let entry =
                                        body_labels.entry(chord.trim().to_string()).or_default();
                                    for l in labels {
                                        entry.insert(l.to_string());
                                    }
                                }
                                continue;
                            }
                        }
                        _ => {}
                    }
                    if depth >= 1 {
                        tuple.push(c);
                    }
                }
            }
            for sk in section_keys(*section) {
                let Some(labels) = body_labels.get(sk.chord) else {
                    continue;
                };
                // Dynamic hints (e.g. the `g` scope toggle) legitimately show
                // several labels; require the table's label to be one of them
                // only when the body uses a single, static label.
                if labels.len() == 1 {
                    let body = labels.iter().next().unwrap();
                    assert_eq!(
                        body, sk.label,
                        "{section:?}: `{}` is \"{}\" in the statusbar table but \
                         \"{body}\" in the in-body hint row",
                        sk.chord, sk.label
                    );
                }
            }
        }
    }
}
