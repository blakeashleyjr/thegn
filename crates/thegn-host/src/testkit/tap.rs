//! TAP (Test Anything Protocol) ingestion — the lingua franca for Bash (bats),
//! Perl (prove), Lua (busted), SQL (pgTAP via pg_prove), node `--test`, and many
//! more. One small parser unlocks a whole family of ecosystems.
//!
//! Grammar we handle (tolerant; non-matching lines are diagnostics or noise):
//!   `ok 1 - desc`, `ok 1 desc`, `not ok 2 - desc`
//!   `ok 3 - desc # SKIP reason`, `not ok 4 - desc # TODO`
//!   `1..N` plan line; `# …` diagnostics (failure location lives here)

use crate::panel::{self, TestLocation, TestNode, TestNodeKind, TestState};

pub fn parse(text: &str) -> Vec<TestNode> {
    let mut nodes: Vec<TestNode> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim_start();
        if let Some(diag) = line.strip_prefix('#') {
            // Attach the first location we see to the most recent failing test.
            if let Some(last) = nodes.last_mut()
                && last.state == TestState::Fail
                && last.location.is_none()
            {
                last.location = tap_location(diag);
            }
            continue;
        }
        let Some((is_ok, rest)) = tap_status(line) else {
            continue;
        };
        let (desc, directive) = split_directive(rest);
        let state = match directive {
            Some(Directive::Skip) | Some(Directive::Todo) => TestState::Skip,
            None if is_ok => TestState::Pass,
            None => TestState::Fail,
        };
        let id = if desc.is_empty() {
            format!("test {}", nodes.len() + 1)
        } else {
            desc.to_string()
        };
        nodes.push(TestNode {
            id: id.clone(),
            label: id,
            depth: 0,
            kind: TestNodeKind::Test,
            state,
            location: None,
            message: None,
            placeholder: false,
        });
    }
    panel::tree_from_flat_tests(nodes)
}

/// Returns `(is_ok, rest_after_status)` for a TAP result line, or `None`.
/// `ok`/`not ok` must be followed by a space, a digit, or end-of-line so we
/// don't match words like "oktober".
fn tap_status(line: &str) -> Option<(bool, &str)> {
    if let Some(rest) = line.strip_prefix("not ok")
        && boundary(rest)
    {
        return Some((false, rest));
    }
    if let Some(rest) = line.strip_prefix("ok")
        && boundary(rest)
    {
        return Some((true, rest));
    }
    None
}

fn boundary(rest: &str) -> bool {
    rest.is_empty()
        || rest
            .chars()
            .next()
            .map(|c| c == ' ' || c.is_ascii_digit())
            .unwrap_or(false)
}

#[derive(PartialEq)]
enum Directive {
    Skip,
    Todo,
}

/// Strip the leading test number and `-` separator, returning `(description,
/// directive)`. A `# SKIP`/`# TODO` directive ends the description.
fn split_directive(rest: &str) -> (&str, Option<Directive>) {
    // Drop leading spaces, an optional number, optional spaces, optional `-`.
    let s = rest.trim_start();
    let s = s
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start();
    let s = s.strip_prefix('-').map(str::trim_start).unwrap_or(s);
    match s.split_once('#') {
        Some((desc, dir)) => {
            let d = dir.trim().to_ascii_lowercase();
            let directive = if d.starts_with("skip") {
                Some(Directive::Skip)
            } else if d.starts_with("todo") {
                Some(Directive::Todo)
            } else {
                None
            };
            (desc.trim(), directive)
        }
        None => (s.trim(), None),
    }
}

/// Pull a `file:line` out of a TAP diagnostic. Handles the three common shapes:
/// `file:line` (busted), `at file line N` (perl), `in test file file, line N`
/// (bats).
fn tap_location(diag: &str) -> Option<TestLocation> {
    if let Some(loc) = panel::extract_locations(diag).into_iter().next() {
        return Some(loc);
    }
    // perl: `at t/foo.t line 7.`   bats: `in test file test.bats, line 6`
    let lower = diag.to_ascii_lowercase();
    let after = lower.find(" line ").map(|i| &diag[i + " line ".len()..])?;
    let line: usize = after
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    // The file is the last whitespace/comma-delimited token before " line ".
    let head = &diag[..lower.find(" line ").unwrap()];
    let file = head
        .rsplit(|c: char| c.is_whitespace() || c == ',')
        .find(|t| !t.is_empty())?;
    Some(TestLocation {
        path: file.trim_end_matches(',').to_string(),
        line,
        column: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_core_tap() {
        let text = "\
TAP version 13
1..4
ok 1 - adds
not ok 2 - breaks
ok 3 - later # SKIP not ready
not ok 4 - known # TODO fixme
";
        let nodes = parse(text);
        let by = |id: &str| nodes.iter().find(|n| n.id == id).cloned();
        assert_eq!(by("adds").unwrap().state, TestState::Pass);
        assert_eq!(by("breaks").unwrap().state, TestState::Fail);
        assert_eq!(by("later").unwrap().state, TestState::Skip);
        assert_eq!(by("known").unwrap().state, TestState::Skip);
    }

    #[test]
    fn attaches_diagnostic_locations() {
        // busted (colon), perl (at … line), bats (in test file … line)
        for (diag, file, line) in [
            ("# spec/calc_spec.lua:5: boom", "spec/calc_spec.lua", 5),
            ("#   at t/basic.t line 7.", "t/basic.t", 7),
            ("# (in test file test.bats, line 6)", "test.bats", 6),
        ] {
            let text = format!("1..1\nnot ok 1 - x\n{diag}\n");
            let nodes = parse(&text);
            let f = nodes.iter().find(|n| n.id == "x").unwrap();
            let loc = f
                .location
                .as_ref()
                .unwrap_or_else(|| panic!("no loc for {diag}"));
            assert_eq!(loc.path, file, "diag {diag}");
            assert_eq!(loc.line, line, "diag {diag}");
        }
    }

    #[test]
    fn bats_style_without_dash() {
        let nodes = parse("1..2\nok 1 adds\nnot ok 2 breaks\n");
        assert_eq!(
            nodes
                .iter()
                .filter(|n| n.kind == TestNodeKind::Test)
                .count(),
            2
        );
        assert!(
            nodes
                .iter()
                .any(|n| n.id == "adds" && n.state == TestState::Pass)
        );
    }
}
