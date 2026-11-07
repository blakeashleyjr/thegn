//! Grouped top-level `--help` for the thegn CLI.
//!
//! clap 4.x cannot group subcommands under headings (clap-rs/clap#1553), so
//! the top-level help installs a custom `help_template` whose commands block
//! is rendered here at runtime from the **live** `clap::Command` tree: names
//! and about-strings come from clap (they cannot drift from the definitions),
//! only the grouping below is ours — and a unit test fails when a visible
//! command is missing from it (or listed twice), so "add a command, forget
//! the group" is a CI failure, not a silent omission.

/// Command-name → help-group table. Hidden commands are never rendered and
/// must NOT be listed here. The built-in `help` subcommand is deliberately
/// omitted from the rendering (it is visible to clap but noise here); the
/// drift test accounts for it.
pub const GROUPS: &[(&str, &[&str])] = &[
    (
        "Projects & worktrees",
        &[
            "wt",
            "repo",
            "program",
            "open",
            "map",
            "land",
            "integrate",
            "merge",
            "search",
        ],
    ),
    (
        "Forge",
        &[
            "pr",
            "issue",
            "dispatch",
            "supervise",
            "kaneo",
            "ci",
            "autopilot",
        ],
    ),
    (
        "Environments",
        &[
            "env",
            "zone",
            "host",
            "placement",
            "sandbox",
            "debug",
            "mcp",
            "plugin",
        ],
    ),
    (
        "Session",
        &[
            "notify",
            "automations",
            "logs",
            "share",
            "forward",
            "sandbox-argv",
            "events",
        ],
    ),
    (
        "Control plane",
        &[
            "serve", "session", "attach", "pair", "api", "proxy", "agent",
        ],
    ),
    (
        "Meta",
        &[
            "voice",
            "config",
            "secret",
            "keys",
            "theme",
            "skills",
            "doctor",
            "db",
            "setup",
            "completions",
        ],
    ),
];

/// Render the grouped Commands block from the live clap command tree.
fn grouped_commands(cmd: &clap::Command) -> String {
    use std::fmt::Write;
    let about = |name: &str| -> Option<String> {
        cmd.get_subcommands()
            .find(|c| !c.is_hide_set() && c.get_name() == name)
            .map(|c| {
                c.get_about()
                    .map(|s| s.to_string())
                    .unwrap_or_default()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            })
    };
    let width = GROUPS
        .iter()
        .flat_map(|(_, names)| names.iter())
        .map(|n| n.len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (heading, names) in GROUPS {
        let mut body = String::new();
        for name in *names {
            if let Some(a) = about(name) {
                let _ = writeln!(body, "  {name:<width$}  {a}"); // best-effort: String formatting cannot fail
            }
        }
        if !body.is_empty() {
            let _ = writeln!(out, "{heading}:"); // best-effort: String formatting cannot fail
            out.push_str(&body);
            out.push('\n');
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// Install the grouped help template on the TOP-LEVEL command only
/// (`help_template` does not propagate; subcommand help keeps clap's default
/// rendering). `{options}` renders the flag list without a heading, so the
/// template carries the literal `Options:` line; `{subcommands}`/`{all-args}`
/// are deliberately absent — the grouped block replaces the flat list.
pub fn attach(cmd: clap::Command) -> clap::Command {
    let mut built = cmd;
    built.build();
    let block = grouped_commands(&built);
    built.help_template(format!(
        "{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{block}\n\nOptions:\n{{options}}{{after-help}}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn built() -> clap::Command {
        let mut cmd = crate::Cli::command();
        cmd.build();
        cmd
    }

    /// Every visible top-level command appears in exactly one group, and every
    /// grouped name exists as a visible command — adding a Command variant
    /// without grouping it (or hiding it) fails here.
    #[test]
    fn groups_cover_visible_commands_exactly() {
        let cmd = built();
        let visible: std::collections::BTreeSet<String> = cmd
            .get_subcommands()
            .filter(|c| !c.is_hide_set())
            .map(|c| c.get_name().to_string())
            // The built-in `help` subcommand is rendered by clap, not us.
            .filter(|n| n != "help")
            .collect();
        let mut grouped = std::collections::BTreeSet::new();
        for (heading, names) in GROUPS {
            for name in *names {
                assert!(
                    grouped.insert(name.to_string()),
                    "'{name}' listed twice in GROUPS (under {heading})"
                );
            }
        }
        assert_eq!(
            grouped, visible,
            "GROUPS and the visible command set must match exactly \
             (left = grouped, right = visible)"
        );
    }

    #[test]
    fn grouped_help_renders_headings_not_hidden_commands() {
        let mut cmd = attach(crate::Cli::command());
        let help = cmd.render_long_help().to_string();
        for (heading, _) in GROUPS {
            assert!(
                help.contains(&format!("{heading}:")),
                "help must contain the '{heading}:' heading\n{help}"
            );
        }
        // Hidden/legacy spellings never render as command rows.
        for legacy in ["\n  list ", "\n  repos ", "\n  recent ", "\n  bridge "] {
            assert!(
                !help.contains(legacy),
                "hidden command leaked into help: {legacy:?}\n{help}"
            );
        }
    }
}

#[cfg(test)]
mod doc_drift {
    use clap::CommandFactory;

    /// docs/cli.md is the public CLI contract (also compiled into the MCP resource).
    const CLI_DOC: &str = include_str!("../../../docs/cli.md");

    /// True when `doc` names `verb` as a command token: the verb is immediately
    /// preceded by a backtick or by `thegn ` and is not followed by an
    /// identifier character, `-` or `_` (so `sandbox` is not satisfied by
    /// `sandbox-argv`, nor `host` by `hosts`).
    fn documented(doc: &str, verb: &str) -> bool {
        doc.match_indices(verb).any(|(start, _)| {
            let bytes = doc.as_bytes();
            let backticked = start > 0 && bytes[start - 1] == b'`';
            let thegn_prefixed = start >= 6 && bytes.get(start - 6..start) == Some(&b"thegn "[..]);
            let end = start + verb.len();
            let followed_by_identifier = doc[end..]
                .chars()
                .next()
                .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_'));
            (backticked || thegn_prefixed) && !followed_by_identifier
        })
    }

    fn visible_top_level_verbs() -> Vec<String> {
        let mut cmd = crate::Cli::command();
        cmd.build();
        cmd.get_subcommands()
            .filter(|subcommand| !subcommand.is_hide_set() && subcommand.get_name() != "help")
            .map(|subcommand| subcommand.get_name().to_string())
            .collect()
    }

    #[test]
    fn documented_matches_backticked_and_thegn_prefixed_tokens() {
        assert!(documented("| `wt list` |", "wt"));
        assert!(documented("thegn host list", "host"));
        assert!(!documented("`sandbox-argv`", "sandbox"));
        assert!(!documented("hosts", "host"));
        assert!(!documented("`dbx`", "db"));
    }

    #[test]
    fn every_visible_top_level_verb_is_documented_in_cli_md() {
        let missing: Vec<String> = visible_top_level_verbs()
            .into_iter()
            .filter(|verb| !documented(CLI_DOC, verb))
            .collect();
        assert!(
            missing.is_empty(),
            "docs/cli.md does not mention these top-level verbs: {missing:?}; add them to the Grammar table"
        );
    }

    #[test]
    fn hidden_legacy_verbs_are_not_required_in_cli_md() {
        let mut cmd = crate::Cli::command();
        cmd.build();
        let hidden: Vec<String> = cmd
            .get_subcommands()
            .filter(|subcommand| subcommand.is_hide_set())
            .map(|subcommand| subcommand.get_name().to_string())
            .collect();
        let visible = visible_top_level_verbs();
        assert!(!hidden.is_empty());
        assert!(hidden.iter().any(|name| name == "daemon"));
        assert!(hidden.iter().any(|name| name == "repos"));
        assert!(hidden.iter().all(|name| !visible.contains(name)));
    }

    #[test]
    fn dev_channel_verbs_are_tagged_in_cli_md() {
        for verb in ["host", "placement"] {
            let token = format!("`{verb}`");
            let row = CLI_DOC
                .lines()
                .find(|line| line.contains(token.as_str()))
                .unwrap_or_else(|| panic!("Grammar table has no row for `{verb}`"));
            assert!(
                row.contains("dev"),
                "Grammar table row for `{verb}` must identify its dev channel: {row}"
            );
        }
    }
}
