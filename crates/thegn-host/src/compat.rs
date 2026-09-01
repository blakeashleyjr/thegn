//! Host-edge compatibility diagnostics for renamed public CLI vocabulary.
//!
//! Clap owns parsing and visible aliases own discoverability. This small raw
//! argv pass supplies the missing deprecation diagnostic without inspecting
//! values (where the word `project` is perfectly valid data).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LegacyAlias {
    ProjectCommand,
    ProjectFlag,
}

fn legacy_alias(args: &[String]) -> Option<LegacyAlias> {
    let mut i = 1;
    let mut command = None;
    while i < args.len() {
        let arg = args[i].as_str();
        if command.is_none() {
            match arg {
                "--config" | "--log-level" | "--profile" | "--set" => {
                    i += 2;
                    continue;
                }
                value
                    if value.starts_with("--config=")
                        || value.starts_with("--log-level=")
                        || value.starts_with("--profile=")
                        || value.starts_with("--set=") =>
                {
                    i += 1;
                    continue;
                }
                value if value.starts_with('-') => {
                    i += 1;
                    continue;
                }
                "project" => return Some(LegacyAlias::ProjectCommand),
                "wt" => command = Some("wt"),
                _ => command = Some(arg),
            }
            i += 1;
            continue;
        }

        if command == Some("wt") && arg == "new" {
            return args[i + 1..]
                .iter()
                .any(|value| value == "--project" || value.starts_with("--project="))
                .then_some(LegacyAlias::ProjectFlag);
        }
        i += 1;
    }
    None
}

/// Warn when an invocation used a public spelling that is retained only for
/// the compatibility window. The warning is stderr-only, so JSON stdout stays
/// byte-for-byte compatible.
#[allow(clippy::disallowed_macros)]
pub(crate) fn warn_legacy_argv(args: &[String]) {
    let Some(alias) = legacy_alias(args) else {
        return;
    };
    let message = match alias {
        LegacyAlias::ProjectCommand => {
            "`thegn project` is deprecated: project was the old name for a multi-repo program; use `thegn program`"
        }
        LegacyAlias::ProjectFlag => "`wt new --project` is deprecated: use `wt new --program`",
    };
    eprintln!("warning: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn detects_only_the_legacy_command_position() {
        assert_eq!(
            legacy_alias(&argv(&["thegn", "project", "list"])),
            Some(LegacyAlias::ProjectCommand)
        );
        assert_eq!(
            legacy_alias(&argv(&["thegn", "program", "create", "project"])),
            None
        );
        assert_eq!(
            legacy_alias(&argv(&["thegn", "wt", "new", "project"])),
            None
        );
    }

    #[test]
    fn detects_the_legacy_batched_flag_only_under_wt_new() {
        assert_eq!(
            legacy_alias(&argv(&["thegn", "wt", "new", "x", "--project", "p"])),
            Some(LegacyAlias::ProjectFlag)
        );
        assert_eq!(
            legacy_alias(&argv(&["thegn", "wt", "list", "--project"])),
            None
        );
        assert_eq!(
            legacy_alias(&argv(&["thegn", "program", "create", "--project"])),
            None
        );
    }

    #[test]
    fn skips_global_option_values() {
        assert_eq!(
            legacy_alias(&argv(&["thegn", "--config", "project", "program", "list"])),
            None
        );
        assert_eq!(
            legacy_alias(&argv(&["thegn", "--set", "foo=project", "program", "list"])),
            None
        );
    }
}
