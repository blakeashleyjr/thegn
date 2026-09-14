//! Parser-only fixtures: no handler, configuration loader or action is invoked.
use super::*;
use clap::Parser;

fn parse(args: &[&str]) -> crate::Cli {
    crate::Cli::try_parse_from(std::iter::once("thegn").chain(args.iter().copied()))
        .unwrap_or_else(|error| panic!("argv {args:?}: {error}"))
}

#[test]
fn every_configured_top_level_variant_parses_without_static_authority() {
    macro_rules! case {
        ($pattern:pat, [$($arg:literal),*]) => {{
            let cli = parse(&[$($arg),*]);
            assert!(matches!(cli.command.as_ref(), Some($pattern)));
            assert_eq!(classify(cli.command.as_ref()), CommandIntent::Configured);
        }};
    }
    case!(Command::Pr { .. }, ["pr", "status"]);
    case!(Command::Issue { .. }, ["issue", "list"]);
    case!(Command::Kaneo { .. }, ["kaneo", "status"]);
    case!(Command::Dispatch { .. }, ["dispatch", "list"]);
    case!(Command::Autopilot { .. }, ["autopilot", "status"]);
    case!(Command::Ci { .. }, ["ci", "runs"]);
    case!(Command::Search(..), ["search", "fixture"]);
    case!(Command::Theme { .. }, ["theme", "list"]);
    case!(Command::Share { .. }, ["share", "list"]);
    case!(Command::Forward { .. }, ["forward", "list"]);
    case!(Command::Wt { .. }, ["wt", "list"]);
    case!(Command::Repo { .. }, ["repo", "list"]);
    case!(
        Command::Open {
            no_launch: true,
            ..
        },
        ["open", "fixture", "--no-launch"]
    );
    case!(Command::Diff { .. }, ["diff"]);
    case!(Command::List { .. }, ["list"]);
    case!(Command::Map { .. }, ["map"]);
    case!(
        Command::Integrate {
            args: cmd::integrate::IntegrateArgs { dry_run: true, .. }
        },
        ["integrate", "--dry-run"]
    );
    case!(Command::Land { .. }, ["land"]);
    case!(Command::Merge { .. }, ["merge", "list"]);
    case!(Command::Disk { .. }, ["disk"]);
    case!(Command::Clean { .. }, ["clean"]);
    case!(Command::Repos { .. }, ["repos"]);
    case!(Command::Recent { .. }, ["recent"]);
    case!(Command::RepoTrust { .. }, ["repo-trust"]);
    case!(Command::Secret { .. }, ["secret", "list"]);
    case!(Command::Proxy { .. }, ["proxy", "status"]);
    case!(Command::Env { .. }, ["env", "list"]);
    case!(Command::Zone { .. }, ["zone", "list"]);
    case!(Command::Project { .. }, ["program", "list"]);
    case!(Command::Placement { .. }, ["placement", "list"]);
    case!(Command::Host { .. }, ["host", "list"]);
    case!(Command::Debug { .. }, ["debug", "path"]);
    case!(Command::Mcp { .. }, ["mcp", "list"]);
    case!(Command::Agent { .. }, ["agent", "list"]);
    case!(Command::Skills { .. }, ["skills", "list"]);
    case!(Command::Plugin { .. }, ["plugin", "list"]);
    case!(Command::SandboxArgv { .. }, ["sandbox-argv"]);
    case!(Command::Sandbox { .. }, ["sandbox", "gc"]);
    case!(Command::Notify { .. }, ["notify", "list"]);
    case!(Command::Logs { .. }, ["logs", "tail"]);
    case!(Command::Keys { .. }, ["keys", "list"]);
    case!(Command::Doctor { .. }, ["doctor"]);
    case!(Command::Setup, ["setup"]);
    case!(Command::Serve { .. }, ["serve"]);
    case!(Command::Session { .. }, ["session", "list"]);
    case!(Command::Events { .. }, ["events", "tail"]);
    case!(Command::Attach { .. }, ["attach"]);
    case!(Command::Pair { .. }, ["pair", "list"]);
    case!(Command::Daemon { .. }, ["daemon"]);
    case!(Command::Bridge, ["bridge"]);
    case!(
        Command::BridgeRevtunnel { .. },
        ["bridge-revtunnel", "12345"]
    );
    case!(Command::SpriteProxy { .. }, ["sprite-proxy"]);
    case!(Command::VpsSsh { .. }, ["vps-ssh", "fixture"]);
    case!(Command::Machine0Ssh { .. }, ["machine0-ssh", "fixture"]);
    case!(Command::SpriteExec { .. }, ["sprite-exec", "fixture"]);
    case!(Command::Project { .. }, ["project", "list"]);
    assert_eq!(
        classify(parse(&[]).command.as_ref()),
        CommandIntent::Configured
    );
}

#[test]
fn every_config_api_and_automation_action_uses_its_exact_intent() {
    macro_rules! case {
        ($pattern:pat, $intent:expr, [$($arg:literal),*]) => {{
            let cli = parse(&[$($arg),*]);
            assert!(matches!(cli.command.as_ref(), Some($pattern)));
            assert_eq!(classify(cli.command.as_ref()), $intent);
        }};
    }
    use CommandIntent::{Configured, Recovery, SourceInspection, Static};
    case!(
        Command::Config {
            action: cmd::config::Action::Path
        },
        Static(StaticIntent::ConfigPath),
        ["config", "path"]
    );
    case!(
        Command::Config {
            action: cmd::config::Action::Schema
        },
        Static(StaticIntent::ConfigSchema),
        ["config", "schema"]
    );
    case!(
        Command::Config {
            action: cmd::config::Action::Show { .. }
        },
        SourceInspection,
        ["config", "show"]
    );
    case!(
        Command::Config {
            action: cmd::config::Action::Get { .. }
        },
        SourceInspection,
        ["config", "get", "theme.accent"]
    );
    case!(
        Command::Config {
            action: cmd::config::Action::Validate { .. }
        },
        SourceInspection,
        ["config", "validate"]
    );
    case!(
        Command::Config {
            action: cmd::config::Action::Explain { .. }
        },
        SourceInspection,
        ["config", "explain", "theme.accent"]
    );
    case!(
        Command::Config {
            action: cmd::config::Action::Edit
        },
        Recovery,
        ["config", "edit"]
    );
    case!(
        Command::Config {
            action: cmd::config::Action::Set { .. }
        },
        Recovery,
        ["config", "set", "theme.accent", "cyan"]
    );
    case!(
        Command::Api {
            action: cmd::api::Action::Call { .. }
        },
        Configured,
        ["api", "call", "worktrees.list"]
    );
    case!(
        Command::Api {
            action: cmd::api::Action::Schema
        },
        Static(StaticIntent::ApiSchema),
        ["api", "schema"]
    );
    case!(
        Command::Automations {
            action: cmd::automations::Action::List { .. }
        },
        Configured,
        ["automations", "list"]
    );
    case!(
        Command::Automations {
            action: cmd::automations::Action::Test { .. }
        },
        SourceInspection,
        ["automations", "test", "missing", "--event", "{}"]
    );
    for json in [false, true] {
        for action in ["list", "coverage"] {
            let mut args = vec!["api", action];
            if json {
                args.push("--json");
            }
            let cli = parse(&args);
            let expected = if action == "list" {
                assert!(
                    matches!(cli.command, Some(Command::Api { action: cmd::api::Action::List { json: actual } }) if actual == json)
                );
                StaticIntent::ApiList { json }
            } else {
                assert!(
                    matches!(cli.command, Some(Command::Api { action: cmd::api::Action::Coverage { json: actual } }) if actual == json)
                );
                StaticIntent::ApiCoverage { json }
            };
            assert_eq!(classify(cli.command.as_ref()), Static(expected));
        }
    }
}

#[test]
fn all_completion_shells_modes_and_global_positions_preserve_borrowed_intent() {
    use clap_complete::Shell;
    for (name, expected) in [
        ("bash", Shell::Bash),
        ("zsh", Shell::Zsh),
        ("fish", Shell::Fish),
        ("elvish", Shell::Elvish),
        ("powershell", Shell::PowerShell),
    ] {
        for static_ in [false, true] {
            for globals_first in [false, true] {
                let globals = [
                    "--config",
                    "relative.toml",
                    "--profile",
                    "private",
                    "--set",
                    "invalid-override",
                ];
                let mut args = Vec::new();
                if globals_first {
                    args.extend(globals);
                }
                args.extend(["completions", name]);
                if static_ {
                    args.push("--static");
                }
                if !globals_first {
                    args.extend(globals);
                }
                let cli = parse(&args);
                let Some(Command::Completions {
                    shell,
                    static_: parsed,
                }) = cli.command.as_ref()
                else {
                    panic!("completion identity");
                };
                assert_eq!((*shell, *parsed), (expected, static_));
                assert_eq!(
                    classify(cli.command.as_ref()),
                    CommandIntent::Static(StaticIntent::Completions { shell, static_ })
                );
                assert_eq!(cli.config.as_deref(), Some(Path::new("relative.toml")));
                assert_eq!(cli.profile.as_deref(), Some("private"));
                assert_eq!(cli.overrides, ["invalid-override"]);
            }
        }
    }
}
