//! Borrowed command intent. Only Static bypasses existing startup/configuration.
//! SourceInspection and Recovery are descriptive tags, not purity guarantees.
use crate::{Command, cmd};
use std::path::Path;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CommandIntent<'a> {
    Static(StaticIntent<'a>),
    SourceInspection,
    Recovery,
    Configured,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StaticIntent<'a> {
    Completions {
        shell: &'a clap_complete::Shell,
        static_: bool,
    },
    ConfigPath,
    ConfigSchema,
    ApiList {
        json: bool,
    },
    ApiCoverage {
        json: bool,
    },
    ApiSchema,
}

pub(crate) fn classify(command: Option<&Command>) -> CommandIntent<'_> {
    match command {
        None => CommandIntent::Configured,
        Some(command) => classify_command(command),
    }
}

fn classify_command(command: &Command) -> CommandIntent<'_> {
    use CommandIntent::{Configured, Recovery, SourceInspection, Static};
    match command {
        Command::Completions { shell, static_ } => Static(StaticIntent::Completions {
            shell,
            static_: *static_,
        }),
        Command::Config { action } => match action {
            cmd::config::Action::Path => Static(StaticIntent::ConfigPath),
            cmd::config::Action::Schema => Static(StaticIntent::ConfigSchema),
            cmd::config::Action::Show { .. }
            | cmd::config::Action::Get { .. }
            | cmd::config::Action::Validate { .. }
            | cmd::config::Action::Explain { .. } => SourceInspection,
            cmd::config::Action::Edit | cmd::config::Action::Set { .. } => Recovery,
        },
        Command::Api { action } => match action {
            cmd::api::Action::List { json } => Static(StaticIntent::ApiList { json: *json }),
            cmd::api::Action::Coverage { json } => {
                Static(StaticIntent::ApiCoverage { json: *json })
            }
            cmd::api::Action::Schema => Static(StaticIntent::ApiSchema),
            cmd::api::Action::Call { .. } => Configured,
        },
        Command::Automations { action } => match action {
            cmd::automations::Action::Test { .. } => SourceInspection,
            cmd::automations::Action::List { .. } => Configured,
        },
        Command::Pr { .. }
        | Command::Issue { .. }
        | Command::Kaneo { .. }
        | Command::Dispatch { .. }
        | Command::Autopilot { .. }
        | Command::Ci { .. }
        | Command::Search(..)
        | Command::Theme { .. }
        | Command::Share { .. }
        | Command::Forward { .. }
        | Command::Wt { .. }
        | Command::Repo { .. }
        | Command::Open { .. }
        | Command::Diff { .. }
        | Command::List { .. }
        | Command::Map { .. }
        | Command::Integrate { .. }
        | Command::Land { .. }
        | Command::Merge { .. }
        | Command::Disk { .. }
        | Command::Clean { .. }
        | Command::Repos { .. }
        | Command::Recent { .. }
        | Command::RepoTrust { .. }
        | Command::Secret { .. }
        | Command::Proxy { .. }
        | Command::Env { .. }
        | Command::Zone { .. }
        | Command::Project { .. }
        | Command::Placement { .. }
        | Command::Host { .. }
        | Command::Debug { .. }
        | Command::Mcp { .. }
        | Command::Agent { .. }
        | Command::Skills { .. }
        | Command::Plugin { .. }
        | Command::SandboxArgv { .. }
        | Command::Sandbox { .. }
        | Command::Notify { .. }
        | Command::Logs { .. }
        | Command::Keys { .. }
        | Command::Doctor { .. }
        | Command::Setup
        | Command::Serve { .. }
        | Command::Session { .. }
        | Command::Events { .. }
        | Command::Attach { .. }
        | Command::Pair { .. }
        | Command::Daemon { .. }
        | Command::Bridge
        | Command::BridgeRevtunnel { .. }
        | Command::SpriteProxy { .. }
        | Command::VpsSsh { .. }
        | Command::Machine0Ssh { .. }
        | Command::SpriteExec { .. } => Configured,
    }
}

pub(crate) fn dispatch_static(
    intent: StaticIntent<'_>,
    explicit_config: Option<&Path>,
) -> anyhow::Result<()> {
    match intent {
        StaticIntent::Completions { shell, static_ } => {
            crate::complete::print_registration(*shell, static_)
        }
        StaticIntent::ConfigPath => {
            match explicit_config {
                Some(path) => cmd::config::print_path(path),
                None => cmd::config::print_path(&thegn_core::config::Config::path()),
            }
            Ok(())
        }
        StaticIntent::ConfigSchema => {
            cmd::config::print_schema();
            Ok(())
        }
        StaticIntent::ApiList { json } => cmd::api::list(json),
        StaticIntent::ApiCoverage { json } => cmd::api::coverage(json),
        StaticIntent::ApiSchema => cmd::api::print_schema(),
    }
}

#[cfg(test)]
#[path = "command_intent_tests.rs"]
mod tests;
