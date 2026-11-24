//! The inbox provider seam (b163, THE-2214): one backend per
//! `[[inbox.accounts]]` kind (`command`, `todoist`, `imap`; gmail, jmap,
//! outlook and slack reserved).
//!
//! The trait is object-safe and read-only in b163: `mark_read` is an optional
//! op whose caps bit is false everywhere (writing needs the outward-approval
//! path, queued). Everything a backend returns is untrusted; the daemon's
//! producer passes it through `thegn_core::inbox::sanitize_item`.
//!
//! Three kinds are implemented, each in its own module: `command`,
//! `todoist` and `imap`. Their secret refs are resolved at fetch time, never
//! at construction. The reserved kinds fail closed as `NotConfigured`.

pub mod command;
pub mod imap;
pub mod imap_proto;
mod todoist;

use futures_util::future::BoxFuture;
use thegn_core::config::{Config, InboxAccount, InboxKind};
use thegn_core::inbox::InboxPage;
use thegn_core::seam::{Availability, Kind, ProbeReport};

/// Why a fetch failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboxError {
    /// The account's kind is reserved, or the account is unusable as written.
    NotConfigured,
    Network(String),
    Auth(String),
    /// The provider answered with an error.
    Api(String),
    /// The command backend's child failed.
    Subprocess(String),
    /// The provider's output could not be parsed.
    Parse(String),
    Timeout,
    /// HTTP 429, with the provider's Retry-After seconds when given.
    RateLimited(Option<u64>),
    /// A response body over the size cap.
    BodyLimit,
    /// An optional op this backend does not support.
    Unsupported(&'static str),
    Io(String),
}

/// How the producer reacts to an [`InboxError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxErrorClass {
    /// Nothing to retry until the config changes.
    Config,
    /// Retry at the next cadence.
    Transient,
    /// Credentials: retry at the next cadence, but surface it.
    Auth,
    /// The provider or its output is broken: retry, but surface it.
    Permanent,
}

impl InboxError {
    /// The error's class.
    pub fn class(&self) -> InboxErrorClass {
        match self {
            InboxError::NotConfigured | InboxError::Unsupported(_) => InboxErrorClass::Config,
            InboxError::Network(_)
            | InboxError::Timeout
            | InboxError::RateLimited(_)
            | InboxError::Io(_) => InboxErrorClass::Transient,
            InboxError::Auth(_) => InboxErrorClass::Auth,
            InboxError::Api(_)
            | InboxError::Subprocess(_)
            | InboxError::Parse(_)
            | InboxError::BodyLimit => InboxErrorClass::Permanent,
        }
    }
}

impl std::fmt::Display for InboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InboxError::NotConfigured => write!(f, "not configured"),
            InboxError::Network(e) => write!(f, "network error: {e}"),
            InboxError::Auth(e) => write!(f, "authentication failed: {e}"),
            InboxError::Api(e) => write!(f, "provider error: {e}"),
            InboxError::Subprocess(e) => write!(f, "{e}"),
            InboxError::Parse(e) => write!(f, "could not parse inbox items: {e}"),
            InboxError::Timeout => write!(f, "inbox fetch timed out"),
            InboxError::RateLimited(Some(s)) => write!(f, "rate limited; retry after {s}s"),
            InboxError::RateLimited(None) => write!(f, "rate limited"),
            InboxError::BodyLimit => write!(f, "provider response exceeded the size limit"),
            InboxError::Unsupported(op) => write!(f, "{op} is not supported by this provider"),
            InboxError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for InboxError {}

/// A backend's optional operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct InboxCaps {
    /// `fetch` honours a cursor and may return `Delta` pages.
    pub cursor: bool,
    /// `mark_read` is implemented (false everywhere in b163).
    pub mark_read: bool,
}

/// A source of inbox items.
pub trait InboxBackend: Send + Sync {
    /// The account kind (`InboxKind::as_str`).
    fn kind(&self) -> &'static str;
    fn caps(&self) -> InboxCaps;
    /// Fetch at most `max_items` items, from `cursor` when the backend has
    /// one (empty = from scratch).
    fn fetch<'a>(
        &'a self,
        cursor: &'a str,
        max_items: usize,
    ) -> BoxFuture<'a, Result<InboxPage, InboxError>>;
    /// Mark one item read upstream. Optional (`caps().mark_read`).
    fn mark_read<'a>(&'a self, _item_id: &'a str) -> BoxFuture<'a, Result<(), InboxError>> {
        Box::pin(async { Err(InboxError::Unsupported("mark_read")) })
    }
}

/// The backend for `account`. A reserved kind is `NotConfigured`; an
/// implemented kind whose account is unusable as written is an error too.
pub fn backend_for(account: &InboxAccount) -> Result<Box<dyn InboxBackend>, InboxError> {
    if account.kind.is_reserved() {
        return Err(InboxError::NotConfigured);
    }
    match account.kind {
        InboxKind::Command => Ok(Box::new(command::CommandBackend::new(account))),
        InboxKind::Todoist => Ok(Box::new(todoist::TodoistBackend::new(account)?)),
        InboxKind::Imap => Ok(Box::new(imap::ImapBackend::from_account(account)?)),
        InboxKind::Gmail | InboxKind::Jmap | InboxKind::Outlook | InboxKind::Slack => {
            Err(InboxError::NotConfigured)
        }
    }
}

/// One probe per `[[inbox.accounts]]` entry (none when the section has no
/// accounts). Cheap and offline: a `which` for a command, a config and
/// secret-ref shape check for the network kinds.
pub fn probes(cfg: &Config) -> Vec<ProbeReport> {
    cfg.inbox
        .accounts
        .iter()
        .map(|account| {
            let id = format!("{}:{}", account.kind.as_str(), account.name);
            if account.kind.is_reserved() {
                return ProbeReport::reserved("inbox", &id);
            }
            let report = match account.kind {
                InboxKind::Command => command::probe(account),
                InboxKind::Todoist => {
                    let parsed = thegn_core::secretref::SecretRef::parse(
                        &account.token,
                        thegn_core::secretref::BareAs::EnvName,
                    );
                    let availability = if parsed.is_configured() {
                        Availability::Ready
                    } else {
                        Availability::Unavailable("no token reference configured".into())
                    };
                    ProbeReport::new("inbox", id, availability)
                        .note(format!("filter: {}", account.filter))
                }
                InboxKind::Imap => imap::probe(account, id),
                InboxKind::Gmail | InboxKind::Jmap | InboxKind::Outlook | InboxKind::Slack => {
                    ProbeReport::reserved("inbox", &id)
                }
            };
            let report = report.with_caps(&InboxCaps::default());
            match (cfg.inbox.enabled, account.enabled) {
                (false, _) => report.note("[inbox] enabled = false; nothing is polled"),
                (true, false) => report.note("account disabled"),
                (true, true) => report,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(name: &str, kind: InboxKind) -> InboxAccount {
        InboxAccount {
            name: name.into(),
            kind,
            command: vec!["sh".into()],
            ..Default::default()
        }
    }

    #[test]
    fn reserved_kinds_are_not_configured() {
        for kind in [
            InboxKind::Gmail,
            InboxKind::Jmap,
            InboxKind::Outlook,
            InboxKind::Slack,
        ] {
            assert_eq!(
                backend_for(&account("x", kind)).err(),
                Some(InboxError::NotConfigured),
                "{kind:?}"
            );
        }
        assert!(
            backend_for(&account("x", InboxKind::Imap)).is_err(),
            "an imap account with no host fails closed"
        );
        assert!(backend_for(&account("x", InboxKind::Command)).is_ok());
        assert!(backend_for(&account("x", InboxKind::Todoist)).is_ok());
    }

    #[test]
    fn probes_one_report_per_account_and_reserved_for_reserved_kinds() {
        let mut cfg = Config::default();
        assert!(probes(&cfg).is_empty());
        cfg.inbox.accounts = vec![
            account("feed", InboxKind::Command),
            account("tasks", InboxKind::Todoist),
            account("mail", InboxKind::Imap),
            account("chat", InboxKind::Slack),
        ];
        cfg.inbox.accounts[0].command = vec!["/definitely/not/here".into()];
        cfg.inbox.accounts[1].token = "env:TODOIST_TEST_TOKEN".into();
        let reports = probes(&cfg);
        assert_eq!(reports.len(), 4);
        assert!(reports.iter().all(|r| r.seam == "inbox"));
        let ids: Vec<_> = reports.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            ids,
            ["command:feed", "todoist:tasks", "imap:mail", "slack:chat"]
        );
        assert!(reports[0].availability.is_unavailable());
        assert!(reports[1].availability.is_ready());
        match &reports[3].availability {
            Availability::Unavailable(why) => assert!(why.contains("reserved"), "{why}"),
            other => panic!("reserved kind reported {other:?}"),
        }
        assert!(
            reports[1]
                .notes
                .iter()
                .any(|n| n.contains("enabled = false")),
            "the section is off by default"
        );
        assert_eq!(reports[1].caps["mark_read"], serde_json::json!(false));
    }

    #[test]
    fn error_classes() {
        use InboxErrorClass::{Auth, Config, Permanent, Transient};
        for (error, class) in [
            (InboxError::NotConfigured, Config),
            (InboxError::Unsupported("mark_read"), Config),
            (InboxError::Network("x".into()), Transient),
            (InboxError::Timeout, Transient),
            (InboxError::RateLimited(Some(3)), Transient),
            (InboxError::BodyLimit, Permanent),
            (InboxError::Io("x".into()), Transient),
            (InboxError::Auth("x".into()), Auth),
            (InboxError::Api("x".into()), Permanent),
            (InboxError::Subprocess("x".into()), Permanent),
            (InboxError::Parse("x".into()), Permanent),
        ] {
            assert_eq!(error.class(), class, "{error:?}");
            assert!(!error.to_string().is_empty());
        }
    }
}
