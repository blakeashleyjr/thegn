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
        "Workspace",
        &["wt", "repo", "open", "land", "integrate", "merge"],
    ),
    ("Forge", &["pr", "issue", "ci"]),
    (
        "Environments",
        &[
            "env",
            "zone",
            "host",
            "placement",
            "agent",
            "proxy",
            "debug",
            "mcp",
        ],
    ),
    (
        "Session",
        &["notify", "logs", "share", "forward", "sandbox-argv"],
    ),
    ("Control plane", &["serve", "session", "pair"]),
    ("Meta", &["config", "theme", "doctor", "completions"]),
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
                let _ = writeln!(body, "  {name:<width$}  {a}");
            }
        }
        if !body.is_empty() {
            let _ = writeln!(out, "{heading}:");
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
