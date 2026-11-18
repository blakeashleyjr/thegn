//! `thegn inbox list|test|status` (b163, THE-2214): the native inbox from the
//! command line. `list` reads `inbox_item` notification rows, `test` fetches
//! one account once (no DB writes, no events), and `status` prints the
//! daemon's `producers.status` reply.
//!
//! Stub (filled by b163 lane 6): every verb refuses.

use anyhow::Result;
use thegn_core::config::Config;

#[derive(clap::Subcommand, Debug, Clone)]
pub enum Action {
    /// List inbox items (notifications of kind inbox_item).
    List {
        /// Only this `[[inbox.accounts]]` entry.
        #[arg(long, value_name = "ACCOUNT")]
        account: Option<String>,
        /// Only unread items.
        #[arg(long)]
        unread: bool,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Fetch one account once and print what it returns (no DB writes, no events).
    Test {
        /// The `[[inbox.accounts]]` name.
        #[arg(value_name = "ACCOUNT")]
        account: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show the daemon's inbox and producer state.
    Status {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Run one `thegn inbox` verb. Stub: refuses.
#[allow(dead_code, reason = "filled by b163 lane 6")]
pub fn run(_cfg: &Config, _action: Action) -> Result<()> {
    anyhow::bail!("not implemented")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(clap::Parser, Debug)]
    struct Cli {
        #[command(subcommand)]
        action: Action,
    }

    #[test]
    fn clap_parses_every_verb() {
        let test = Cli::try_parse_from(["inbox", "test", "work", "--json"]).unwrap();
        assert!(matches!(
            test.action,
            Action::Test { ref account, json: true } if account == "work"
        ));
        let list = Cli::try_parse_from(["inbox", "list", "--account", "mail", "--unread"]).unwrap();
        assert!(matches!(
            list.action,
            Action::List { account: Some(ref a), unread: true, json: false } if a == "mail"
        ));
        let status = Cli::try_parse_from(["inbox", "status"]).unwrap();
        assert!(matches!(status.action, Action::Status { json: false }));
        assert!(
            Cli::try_parse_from(["inbox", "test"]).is_err(),
            "ACCOUNT is required"
        );
    }

    #[test]
    fn the_stub_refuses() {
        let err = run(&Config::default(), Action::Status { json: true }).unwrap_err();
        assert_eq!(err.to_string(), "not implemented");
    }
}
