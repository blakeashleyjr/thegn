//! An upgrade left an older thegn daemon running on the same state directory, so this
//! build cannot migrate the database. Detect that typed refusal, explain it in plain
//! words, and offer a one-step restart (`thegn daemon stop`). Never signals anything
//! itself: stopping goes through the verified `daemon stop` path.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

use thegn_core::db::{MigrationAccessError, STOP_DAEMON_COMMAND};

/// The refusal, as the host sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StaleDaemon {
    pub database: PathBuf,
}

/// Whether `error` (anywhere in its context chain) is the old-daemon refusal.
pub(crate) fn detect(error: &anyhow::Error) -> Option<StaleDaemon> {
    match error.downcast_ref::<MigrationAccessError>() {
        Some(MigrationAccessError::OldSchemaInUse { database }) => Some(StaleDaemon {
            database: database.clone(),
        }),
        _ => None,
    }
}

/// Plain-words explanation shared by the CLI and the TUI banner.
pub(crate) fn explanation(stale: &StaleDaemon) -> String {
    format!(
        "thegn was upgraded, but an older thegn is still running in the background (its daemon keeps your panes alive across quits). \
         It is holding the state database ({}) on the old version, so this build cannot update it yet.",
        stale.database.display()
    )
}

/// One-line TUI banner.
pub(crate) fn banner() -> String {
    format!(
        "OLD DAEMON: an older thegn is still running; quit, run `{STOP_DAEMON_COMMAND}` (ends its panes), then restart thegn"
    )
}

/// What to do after explaining.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Offer {
    /// A person is at a terminal: ask before stopping.
    Ask,
    /// Scripts and pipes: print the exact command and change nothing.
    PrintCommand,
}

pub(crate) fn offer_for(interactive: bool) -> Offer {
    if interactive {
        Offer::Ask
    } else {
        Offer::PrintCommand
    }
}

/// Only an explicit yes counts; anything else (including EOF) declines.
pub(crate) fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Outcome of handling the refusal at the CLI.
pub(crate) enum Outcome {
    /// The daemon was stopped; the user's command should simply be run again.
    Stopped,
    /// Nothing was changed.
    NotStopped,
}

/// Explain on stderr, then ask (interactive) or print the command (non-interactive).
#[allow(clippy::disallowed_macros)] // CLI prompt on stderr before the TUI starts
#[expect(
    clippy::disallowed_methods,
    reason = "CLI refusal path runs before the event loop starts; waits on `daemon stop`"
)]
pub(crate) fn report(stale: &StaleDaemon) -> Outcome {
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    eprintln!("thegn: {}", explanation(stale));
    match offer_for(interactive) {
        Offer::PrintCommand => {
            eprintln!(
                "To fix it, run:  {STOP_DAEMON_COMMAND}   (this ends that instance's panes), then run thegn again."
            );
            Outcome::NotStopped
        }
        Offer::Ask => {
            eprint!("Stop it now? This ends that instance's panes. [y/N] ");
            let _ = std::io::stderr().flush(); // best-effort: prompt text only
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line); // best-effort: EOF or error declines
            if !is_yes(&line) {
                eprintln!("Not stopped. To do it later, run:  {STOP_DAEMON_COMMAND}");
                return Outcome::NotStopped;
            }
            match std::env::current_exe().and_then(|exe| {
                std::process::Command::new(exe)
                    .args(["daemon", "stop"])
                    .status()
            }) {
                Ok(status) if status.success() => {
                    eprintln!("Stopped. Run thegn again to finish the upgrade.");
                    Outcome::Stopped
                }
                Ok(status) => {
                    eprintln!(
                        "`{STOP_DAEMON_COMMAND}` exited with {status}. Run it yourself to see why."
                    );
                    Outcome::NotStopped
                }
                Err(error) => {
                    eprintln!("could not run `{STOP_DAEMON_COMMAND}`: {error}. Run it yourself.");
                    Outcome::NotStopped
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_typed_refusal_even_through_context() {
        let error: anyhow::Error = MigrationAccessError::OldSchemaInUse {
            database: PathBuf::from("/s/thegn.db"),
        }
        .into();
        let wrapped = error.context("open state db");
        assert_eq!(
            detect(&wrapped),
            Some(StaleDaemon {
                database: PathBuf::from("/s/thegn.db")
            })
        );
    }

    #[test]
    fn other_failures_are_not_a_stale_daemon() {
        assert_eq!(detect(&anyhow::anyhow!("disk full")), None);
        let newer: anyhow::Error = MigrationAccessError::NewerSchema {
            observed: 9,
            supported: 8,
        }
        .into();
        assert_eq!(detect(&newer), None);
    }

    #[test]
    fn offer_asks_only_when_interactive() {
        assert_eq!(offer_for(true), Offer::Ask);
        assert_eq!(offer_for(false), Offer::PrintCommand);
    }

    #[test]
    fn only_an_explicit_yes_confirms() {
        assert!(is_yes("y\n") && is_yes(" YES ") && !is_yes("") && !is_yes("n") && !is_yes("yep"));
    }

    #[test]
    fn explanation_and_banner_name_the_exact_command() {
        let stale = StaleDaemon {
            database: PathBuf::from("/s/thegn.db"),
        };
        assert!(explanation(&stale).contains("/s/thegn.db"));
        assert!(banner().contains("thegn daemon stop"));
    }
}
