//! Shared `--worktree` scope-selector arg groups — the canonical worktree
//! targeting grammar (see docs/cli.md "Worktree targeting").
//!
//! Two shapes:
//! - [`WorktreeFlag`] — for verbs that have always taken the `--worktree`
//!   flag; flattening it single-sources the flag's help text.
//! - [`WorktreeTarget`] — for verbs that historically took a trailing
//!   positional (`env`, `placement plan`, `merge rm|land`, `land`,
//!   `sandbox-argv`). The flag is canonical; the positional stays parseable
//!   for back-compat but is hidden from help (silent in alpha; removal to be
//!   announced for beta). Passing both is a clap usage error
//!   (`conflicts_with`), which exits non-zero.
//!
//! Verbs whose `--worktree` has *different* semantics keep their own bespoke
//! arg and doc: `wt disk` (default: ALL known worktrees), `wt clean`
//! (interacts with `--all`), and `placement explain` (no-arg means "the most
//! recent decision overall" — its inline pair lives in `cmd/placement.rs`).

/// The canonical `--worktree` scope flag, for flattening into flag-style verbs.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct WorktreeFlag {
    /// Worktree to target (default: the current worktree — $THEGN_WORKTREE,
    /// else the git toplevel of the cwd).
    #[arg(long)]
    pub worktree: Option<String>,
}

/// The canonical `--worktree` scope flag plus the hidden legacy trailing
/// positional, for the verbs that historically took a positional worktree.
/// Flatten this LAST in a variant so the hidden positional keeps its trailing
/// index behind any existing required positionals.
#[derive(clap::Args, Clone, Debug, Default)]
pub struct WorktreeTarget {
    /// Worktree to target (default: the current worktree — $THEGN_WORKTREE,
    /// else the git toplevel of the cwd).
    #[arg(long)]
    pub worktree: Option<String>,
    /// Hidden legacy spelling: the trailing positional worktree. Deprecated —
    /// kept parsing so existing alpha scripts don't break.
    #[arg(value_name = "WORKTREE", hide = true, conflicts_with = "worktree")]
    pub worktree_pos: Option<String>,
}

impl WorktreeTarget {
    /// The explicitly requested worktree, if any. The flag wins over the
    /// legacy positional (unreachable through clap, which rejects both-given
    /// via `conflicts_with`; the precedence matters only for direct
    /// construction).
    pub fn get(self) -> Option<String> {
        self.worktree.or(self.worktree_pos)
    }
}

#[cfg(test)]
mod tests {
    use super::WorktreeTarget;
    use crate::cmd;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<crate::Cli, clap::Error> {
        crate::Cli::try_parse_from(args)
    }

    /// `unwrap_err` without requiring `Debug` on `Cli`.
    fn parse_err(args: &[&str]) -> clap::Error {
        match parse(args) {
            Err(e) => e,
            Ok(_) => panic!("expected a parse error for {args:?}"),
        }
    }

    /// Pull the [`WorktreeTarget`] out of a parsed CLI, whatever verb carried it.
    fn target_of(cli: crate::Cli) -> WorktreeTarget {
        match cli.command.expect("a subcommand") {
            crate::Command::Land { target } => target,
            crate::Command::SandboxArgv { target } => target,
            crate::Command::Merge { action } => match action {
                cmd::merge::Action::Rm { target } => target,
                cmd::merge::Action::Land { target } => target,
                _ => panic!("unexpected merge action"),
            },
            crate::Command::Env { action } => match action {
                cmd::env::Action::Show { target } => target,
                cmd::env::Action::Set { target, .. } => target,
                cmd::env::Action::Forward { target, .. } => target,
                cmd::env::Action::Deprovision { target, .. } => target,
                cmd::env::Action::Restore { target, .. } => target,
                _ => panic!("unexpected env action"),
            },
            crate::Command::Placement { action } => match action {
                cmd::placement::Action::Plan { target, .. } => target,
                _ => panic!("unexpected placement action"),
            },
            _ => panic!("verb without a WorktreeTarget"),
        }
    }

    #[test]
    fn flag_form_parses() {
        let t = target_of(parse(&["thegn", "env", "show", "--worktree", "/x"]).unwrap());
        assert_eq!(t.worktree.as_deref(), Some("/x"));
        assert_eq!(t.worktree_pos, None);
        assert_eq!(t.get().as_deref(), Some("/x"));
    }

    #[test]
    fn legacy_positional_still_parses() {
        let t = target_of(parse(&["thegn", "env", "show", "/x"]).unwrap());
        assert_eq!(t.worktree, None);
        assert_eq!(t.worktree_pos.as_deref(), Some("/x"));
        assert_eq!(t.get().as_deref(), Some("/x"));
    }

    #[test]
    fn both_forms_is_a_usage_error() {
        let err = parse_err(&["thegn", "env", "show", "--worktree", "/x", "/y"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn flag_wins_on_direct_construction() {
        // Unreachable through clap (conflicts_with), but .get()'s precedence
        // must stay flag-first for any programmatic construction.
        let t = WorktreeTarget {
            worktree: Some("/flag".into()),
            worktree_pos: Some("/pos".into()),
        };
        assert_eq!(t.get().as_deref(), Some("/flag"));
    }

    #[test]
    fn hidden_positional_absent_from_help() {
        let err = parse_err(&["thegn", "env", "show", "--help"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = err.to_string();
        assert!(
            help.contains("--worktree"),
            "flag missing from help:\n{help}"
        );
        // The positional would render as a bare `[WORKTREE]` in usage/args.
        assert!(
            !help.contains("[WORKTREE]"),
            "hidden positional leaked into help:\n{help}"
        );
    }

    #[test]
    fn trailing_positional_stacks_behind_required_positionals() {
        // `env set NAME [WORKTREE]`
        let cli = parse(&["thegn", "env", "set", "company-k8s", "/x"]).unwrap();
        match cli.command.unwrap() {
            crate::Command::Env {
                action: cmd::env::Action::Set { name, target, .. },
            } => {
                assert_eq!(name, "company-k8s");
                assert_eq!(target.get().as_deref(), Some("/x"));
            }
            _ => panic!("bad parse"),
        }
        // `env restore ID [WORKTREE]`
        let cli = parse(&["thegn", "env", "restore", "cp1", "/x"]).unwrap();
        match cli.command.unwrap() {
            crate::Command::Env {
                action: cmd::env::Action::Restore { id, target },
            } => {
                assert_eq!(id, "cp1");
                assert_eq!(target.get().as_deref(), Some("/x"));
            }
            _ => panic!("bad parse"),
        }
        // `env forward SPEC [WORKTREE]`
        let cli = parse(&["thegn", "env", "forward", "80:80", "/x"]).unwrap();
        match cli.command.unwrap() {
            crate::Command::Env {
                action: cmd::env::Action::Forward { spec, target },
            } => {
                assert_eq!(spec, "80:80");
                assert_eq!(target.get().as_deref(), Some("/x"));
            }
            _ => panic!("bad parse"),
        }
        // `env deprovision [ID] [WORKTREE]` — the legacy double-optional stack.
        let cli = parse(&["thegn", "env", "deprovision", "someid", "/x"]).unwrap();
        match cli.command.unwrap() {
            crate::Command::Env {
                action: cmd::env::Action::Deprovision { id, target, .. },
            } => {
                assert_eq!(id.as_deref(), Some("someid"));
                assert_eq!(target.get().as_deref(), Some("/x"));
            }
            _ => panic!("bad parse"),
        }
    }

    #[test]
    fn single_positional_verbs_parse_both_forms() {
        for args in [
            &["thegn", "placement", "plan", "/x", "--json"][..],
            &["thegn", "merge", "rm", "/x"][..],
            &["thegn", "land", "/x"][..],
            &["thegn", "sandbox-argv", "/x"][..],
        ] {
            let t = target_of(parse(args).unwrap());
            assert_eq!(t.get().as_deref(), Some("/x"), "positional for {args:?}");
        }
        for args in [
            &["thegn", "placement", "plan", "--worktree", "/x", "--json"][..],
            &["thegn", "merge", "rm", "--worktree", "/x"][..],
            &["thegn", "land", "--worktree", "/x"][..],
            &["thegn", "sandbox-argv", "--worktree", "/x"][..],
        ] {
            let t = target_of(parse(args).unwrap());
            assert_eq!(t.get().as_deref(), Some("/x"), "flag for {args:?}");
        }
    }

    #[test]
    fn placement_explain_no_arg_stays_none() {
        // `placement explain` with no worktree means "the most recent decision
        // overall" — the parsed Option must stay None (never resolved to cwd).
        let cli = parse(&["thegn", "placement", "explain"]).unwrap();
        match cli.command.unwrap() {
            crate::Command::Placement {
                action:
                    cmd::placement::Action::Explain {
                        worktree,
                        worktree_pos,
                        ..
                    },
            } => {
                assert_eq!(worktree, None);
                assert_eq!(worktree_pos, None);
            }
            _ => panic!("bad parse"),
        }
        // Both spellings still select a worktree filter when given.
        let cli = parse(&["thegn", "placement", "explain", "--worktree", "/x"]).unwrap();
        match cli.command.unwrap() {
            crate::Command::Placement {
                action: cmd::placement::Action::Explain { worktree, .. },
            } => assert_eq!(worktree.as_deref(), Some("/x")),
            _ => panic!("bad parse"),
        }
        let cli = parse(&["thegn", "placement", "explain", "/x"]).unwrap();
        match cli.command.unwrap() {
            crate::Command::Placement {
                action: cmd::placement::Action::Explain { worktree_pos, .. },
            } => assert_eq!(worktree_pos.as_deref(), Some("/x")),
            _ => panic!("bad parse"),
        }
    }
}
