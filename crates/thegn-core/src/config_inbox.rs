//! The `[inbox]` config family (THE-2214): the native inbox's accounts (mail,
//! Todoist, any command). Kept beside `config.rs` per the keep-god-files-flat
//! guidance; `config.rs` re-exports everything here.
//!
//! `[inbox]` is global config only. It is in no repo, workspace or profile
//! overlay, Lead rules cannot set it, and `inbox` is a security-relevant root
//! (an unknown key under it refuses). Credentials (`token`, `password`, `env`
//! values) are secret refs resolved by the daemon at fetch time; a literal
//! `token`/`password` is refused by `thegn config validate`.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

use crate::config::{config_enum, config_warn};
use crate::secretref::{BareAs, SecretRef};

/// The floor on any inbox poll interval, in seconds. Applied in
/// [`InboxAccount::effective_poll_secs`], so a `0` can never spin a poll loop.
pub const MIN_POLL_SECS: u64 = 60;
/// The most items one account fetch may bring in.
pub const MAX_ITEMS_CAP: usize = 500;
/// The longest a single account fetch may run, in seconds.
pub const MAX_TIMEOUT_SECS: u64 = 120;
/// The longest account name.
const MAX_NAME_CHARS: usize = 40;

config_enum! {
    /// Where an inbox account's items come from.
    pub enum InboxKind : "inbox kind" {
        Command = "command" | "exec",
        Todoist = "todoist",
        Imap = "imap",
        Gmail = "gmail" reserved,
        Jmap = "jmap" reserved,
        Outlook = "outlook" reserved,
        Slack = "slack" reserved,
    } default = Command;
}

/// `[inbox]` — the native inbox. Off by default; the daemon polls it only
/// when `[daemon] enabled` and `headless_producers` are on too.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct InboxConfig {
    // --- scalars (must serialize before any array of tables for TOML) ---
    /// Master switch. Enabling it is the consent step for any polling.
    pub enabled: bool,
    /// Default seconds between polls, for accounts that do not set their own.
    /// Floored at 60.
    pub poll_interval_secs: u64,

    // --- arrays of tables ---
    /// `[[inbox.accounts]]` — where items come from.
    pub accounts: Vec<InboxAccount>,
}

impl Default for InboxConfig {
    fn default() -> Self {
        InboxConfig {
            enabled: false,
            poll_interval_secs: 300,
            accounts: Vec::new(),
        }
    }
}

/// A `[[inbox.accounts]]` entry: one item source.
///
/// One flat struct with the union of every kind's fields (like
/// `[[calendar.accounts]]`); only the fields relevant to `kind` are read.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct InboxAccount {
    /// Stable id for this account, e.g. `"work"`: lowercase letters, digits,
    /// `_` and `-`, at most 40 characters. It is the state key and appears in
    /// every item's message (`[work] ...`).
    pub name: String,
    /// Which backend this account talks to.
    pub kind: InboxKind,
    /// Poll this account? Disabled entries stay in config but are skipped.
    pub enabled: bool,
    /// Seconds between polls; `0` inherits `[inbox] poll_interval_secs`.
    /// Always floored at 60.
    pub poll_interval_secs: u64,
    /// The most items one fetch may return (1 to 500).
    pub max_items: usize,
    /// Seconds before a fetch is abandoned (1 to 120).
    pub timeout_secs: u64,

    /// `command`: the argv to run (no shell). It prints the items as JSON.
    pub command: Vec<String>,
    /// `command`: working directory; empty means the daemon's.
    pub cwd: String,
    /// `command`: extra environment. Values are secret refs
    /// (`keyring:`/`env:`/`file:`) or plain strings.
    pub env: BTreeMap<String, String>,

    /// `todoist`: the API token, as a secret ref (`keyring:`/`env:`/`file:`).
    pub token: String,
    /// `todoist`: the task filter.
    pub filter: String,
    /// `todoist`: the API base URL (https; http only for a loopback host).
    pub base_url: String,

    /// `imap`: the server host.
    pub host: String,
    /// `imap`: the server port (TLS).
    pub port: u16,
    /// `imap`: the login name.
    pub username: String,
    /// `imap`: the password, as a secret ref (`keyring:`/`env:`/`file:`).
    pub password: String,
    /// `imap`: the mailbox to read.
    pub mailbox: String,
    /// `imap`: only unread messages.
    pub unseen_only: bool,
}

impl Default for InboxAccount {
    fn default() -> Self {
        InboxAccount {
            name: String::new(),
            kind: InboxKind::Command,
            // True so an entry that omits it is polled.
            enabled: true,
            poll_interval_secs: 0,
            max_items: 50,
            timeout_secs: 30,
            command: Vec::new(),
            cwd: String::new(),
            env: BTreeMap::new(),
            token: String::new(),
            filter: "today | overdue".to_owned(),
            base_url: "https://api.todoist.com".to_owned(),
            host: String::new(),
            port: 993,
            username: String::new(),
            password: String::new(),
            mailbox: "INBOX".to_owned(),
            unseen_only: true,
        }
    }
}

const REDACTED: &str = "<redacted>";

fn redact_set(value: &str) -> &str {
    if value.is_empty() { "" } else { REDACTED }
}

impl std::fmt::Debug for InboxAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Credentials (and the login name) never reach a log line or a
        // formatted error through Debug.
        f.debug_struct("InboxAccount")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("enabled", &self.enabled)
            .field("poll_interval_secs", &self.poll_interval_secs)
            .field("max_items", &self.max_items)
            .field("timeout_secs", &self.timeout_secs)
            .field("command", &self.command)
            .field("cwd", &self.cwd)
            .field(
                "env",
                &self
                    .env
                    .keys()
                    .map(|key| (key.as_str(), REDACTED))
                    .collect::<BTreeMap<_, _>>(),
            )
            .field("token", &redact_set(&self.token))
            .field("filter", &self.filter)
            .field("base_url", &self.base_url)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &redact_set(&self.username))
            .field("password", &redact_set(&self.password))
            .field("mailbox", &self.mailbox)
            .field("unseen_only", &self.unseen_only)
            .finish()
    }
}

/// True for `^[a-z0-9][a-z0-9_-]{0,39}$`.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_lowercase() || first.is_ascii_digit())
        && name.len() <= MAX_NAME_CHARS
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// True when `host` is `localhost` or a loopback IP literal.
fn loopback_host(host: &str) -> bool {
    let ip = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || ip
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// `None` when `url` is https, or http to a loopback host; else the problem.
fn base_url_problem(url: &str) -> Option<&'static str> {
    let Ok(parsed) = url::Url::parse(url.trim()) else {
        return Some("is not a valid URL");
    };
    match parsed.scheme() {
        "https" if parsed.host_str().is_some() => None,
        "http" if parsed.host_str().is_some_and(loopback_host) => None,
        "http" => Some("must be https (http only for a loopback host)"),
        _ => Some("must be an https URL"),
    }
}

impl InboxConfig {
    /// Every semantic problem, as `inbox.accounts[i].<key>: ...` strings.
    pub fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        let mut names = BTreeSet::new();
        for (i, account) in self.accounts.iter().enumerate() {
            let at = format!("inbox.accounts[{i}]");
            if !valid_name(&account.name) {
                errs.push(format!(
                    "{at}.name: {:?} must match ^[a-z0-9][a-z0-9_-]{{0,39}}$",
                    account.name
                ));
            } else if !names.insert(account.name.as_str()) {
                errs.push(format!(
                    "{at}.name: duplicate account name {:?}",
                    account.name
                ));
            }
            if <InboxKind as crate::seam::Kind>::is_reserved(account.kind) {
                errs.push(format!(
                    "{at}.kind: inbox kind {:?} is reserved: accepted by config but not implemented in this build",
                    account.kind.as_str()
                ));
            }
            match account.kind {
                InboxKind::Command => {
                    match account.command.first() {
                        None => errs.push(format!("{at}.command: must not be empty")),
                        Some(argv0) if argv0.trim().is_empty() => {
                            errs.push(format!("{at}.command[0]: must not be blank"));
                        }
                        Some(_) => {}
                    }
                    if account.command.iter().any(|arg| arg.contains('\0'))
                        || account.cwd.contains('\0')
                    {
                        errs.push(format!("{at}.command: must not contain a NUL byte"));
                    }
                }
                InboxKind::Todoist => {
                    if account.token.trim().is_empty() {
                        errs.push(format!("{at}.token: required for kind = \"todoist\""));
                    }
                    if account.base_url.trim().is_empty() {
                        errs.push(format!("{at}.base_url: required for kind = \"todoist\""));
                    }
                }
                InboxKind::Imap => {
                    for (key, value) in [
                        ("host", &account.host),
                        ("username", &account.username),
                        ("password", &account.password),
                    ] {
                        if value.trim().is_empty() {
                            errs.push(format!("{at}.{key}: required for kind = \"imap\""));
                        }
                    }
                    if account.port == 0 {
                        errs.push(format!("{at}.port: must not be 0"));
                    }
                }
                InboxKind::Gmail | InboxKind::Jmap | InboxKind::Outlook | InboxKind::Slack => {}
            }
            for (key, value) in [("token", &account.token), ("password", &account.password)] {
                if value.trim().is_empty() {
                    continue;
                }
                let parsed = SecretRef::parse(value, BareAs::Literal);
                if parsed.is_literal() || !parsed.is_configured() {
                    errs.push(format!(
                        "{at}.{key}: must be a secret ref (keyring:/env:/file:)"
                    ));
                }
            }
            if (account.kind == InboxKind::Todoist || !account.base_url.trim().is_empty())
                && let Some(problem) = base_url_problem(&account.base_url)
            {
                errs.push(format!("{at}.base_url: {problem}"));
            }
            if !(1..=MAX_ITEMS_CAP).contains(&account.max_items) {
                errs.push(format!(
                    "{at}.max_items: {} is outside 1..={MAX_ITEMS_CAP}",
                    account.max_items
                ));
            }
            if !(1..=MAX_TIMEOUT_SECS).contains(&account.timeout_secs) {
                errs.push(format!(
                    "{at}.timeout_secs: {} is outside 1..={MAX_TIMEOUT_SECS}",
                    account.timeout_secs
                ));
            }
        }
        errs
    }

    /// The enabled accounts of an implemented (non-reserved) kind. Does not
    /// read the section's own `enabled` switch.
    pub fn implemented_enabled(&self) -> impl Iterator<Item = &InboxAccount> {
        self.accounts.iter().filter(|account| {
            account.enabled && !<InboxKind as crate::seam::Kind>::is_reserved(account.kind)
        })
    }
}

impl InboxAccount {
    /// Seconds between this account's polls: its own interval, or `global`
    /// when it is `0`, never below [`MIN_POLL_SECS`].
    pub fn effective_poll_secs(&self, global: u64) -> u64 {
        let own = if self.poll_interval_secs == 0 {
            global
        } else {
            self.poll_interval_secs
        };
        own.max(MIN_POLL_SECS)
    }

    /// A stable hash of the kind and the non-secret identity fields. A change
    /// means "a different source": the next poll is a fresh baseline. Secret
    /// values (`token`, `password`, `env` values) never enter it, so rotating
    /// a credential does not re-baseline.
    pub fn config_digest(&self) -> String {
        let mut hasher = Sha256::new();
        let mut field = |label: &str, value: &str| {
            for part in [label, value] {
                hasher.update((part.len() as u64).to_le_bytes());
                hasher.update(part.as_bytes());
            }
        };
        field("kind", self.kind.as_str());
        field("argc", &self.command.len().to_string());
        for arg in &self.command {
            field("arg", arg);
        }
        field("cwd", &self.cwd);
        field("filter", &self.filter);
        field("base_url", &self.base_url);
        field("host", &self.host);
        field("port", &self.port.to_string());
        field("username", &self.username);
        field("mailbox", &self.mailbox);
        field("unseen_only", if self.unseen_only { "1" } else { "0" });
        let digest = hasher.finalize();
        let mut out = String::with_capacity(32);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in &digest[..16] {
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(name: &str) -> InboxAccount {
        InboxAccount {
            name: name.into(),
            command: vec!["inbox-feed".into(), "--json".into()],
            ..Default::default()
        }
    }

    fn cfg(accounts: Vec<InboxAccount>) -> InboxConfig {
        InboxConfig {
            enabled: true,
            accounts,
            ..Default::default()
        }
    }

    fn one_error(account: InboxAccount) -> String {
        let errs = cfg(vec![account]).validate();
        assert_eq!(errs.len(), 1, "{errs:?}");
        errs[0].clone()
    }

    #[test]
    fn defaults_are_off_and_sane() {
        let c = InboxConfig::default();
        assert!(!c.enabled);
        assert_eq!(c.poll_interval_secs, 300);
        assert!(c.accounts.is_empty());
        assert!(c.validate().is_empty());
        let a = InboxAccount::default();
        assert_eq!(a.kind, InboxKind::Command);
        assert!(a.enabled);
        assert_eq!((a.max_items, a.timeout_secs, a.port), (50, 30, 993));
        assert_eq!(a.filter, "today | overdue");
        assert_eq!(a.base_url, "https://api.todoist.com");
        assert_eq!(a.mailbox, "INBOX");
        assert!(a.unseen_only);
    }

    #[test]
    fn parses_every_kind_from_toml() {
        let parsed: InboxConfig = toml::from_str(
            r#"
            enabled = true
            poll_interval_secs = 120
            [[accounts]]
            name = "feed"
            kind = "exec"
            command = ["feed", "--json"]
            env = { TOKEN = "env:FEED_TOKEN" }
            [[accounts]]
            name = "tasks"
            kind = "todoist"
            token = "keyring:todoist"
            [[accounts]]
            name = "mail"
            kind = "imap"
            host = "imap.example.com"
            username = "me"
            password = "env:IMAP_PASS"
            unseen_only = false
            "#,
        )
        .unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.poll_interval_secs, 120);
        let kinds: Vec<_> = parsed.accounts.iter().map(|a| a.kind).collect();
        assert_eq!(
            kinds,
            [InboxKind::Command, InboxKind::Todoist, InboxKind::Imap]
        );
        assert_eq!(parsed.accounts[2].port, 993);
        assert!(!parsed.accounts[2].unseen_only);
        assert!(parsed.validate().is_empty(), "{:?}", parsed.validate());
        assert_eq!(parsed.implemented_enabled().count(), 3);
    }

    #[test]
    fn unknown_keys_are_refused() {
        assert!(toml::from_str::<InboxConfig>("bogus = 1").is_err());
        assert!(toml::from_str::<InboxConfig>("[[accounts]]\nname = \"a\"\nnope = 1").is_err());
    }

    #[test]
    fn names_must_be_unique_and_well_formed() {
        assert!(
            cfg(vec![command("work"), command("w0rk_2-x")])
                .validate()
                .is_empty()
        );
        let errs = cfg(vec![command("work"), command("work")]).validate();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("duplicate"), "{errs:?}");
        let too_long = "a".repeat(41);
        for bad in [
            "",
            "Work",
            "-work",
            "_w",
            "wo rk",
            "wörk",
            too_long.as_str(),
        ] {
            let err = one_error(command(bad));
            assert!(err.contains(".name:"), "{bad:?}: {err}");
        }
        assert!(cfg(vec![command(&"a".repeat(40))]).validate().is_empty());
    }

    #[test]
    fn reserved_kinds_are_refused() {
        for kind in [
            InboxKind::Gmail,
            InboxKind::Jmap,
            InboxKind::Outlook,
            InboxKind::Slack,
        ] {
            let err = one_error(InboxAccount {
                kind,
                ..command("x")
            });
            assert!(err.contains("is reserved"), "{err}");
        }
        let c = cfg(vec![InboxAccount {
            kind: InboxKind::Gmail,
            ..command("x")
        }]);
        assert_eq!(c.implemented_enabled().count(), 0);
    }

    #[test]
    fn command_argv_is_checked() {
        let mut a = command("c");
        a.command.clear();
        assert!(one_error(a).contains("must not be empty"));
        let mut a = command("c");
        a.command[0] = "  ".into();
        assert!(one_error(a).contains("must not be blank"));
        let mut a = command("c");
        a.command[1] = "a\0b".into();
        assert!(one_error(a).contains("NUL"));
    }

    #[test]
    fn todoist_needs_a_secret_ref_token() {
        let base = InboxAccount {
            name: "t".into(),
            kind: InboxKind::Todoist,
            ..Default::default()
        };
        assert!(one_error(base.clone()).contains("token: required"));
        let literal = InboxAccount {
            token: "0123456789abcdef".into(),
            ..base.clone()
        };
        let err = one_error(literal);
        assert!(
            err.contains("must be a secret ref (keyring:/env:/file:)"),
            "{err}"
        );
        assert!(
            !err.contains("0123456789abcdef"),
            "never echo the value: {err}"
        );
        let empty_ref = InboxAccount {
            token: "env:".into(),
            ..base.clone()
        };
        assert!(one_error(empty_ref).contains("secret ref"));
        for good in ["env:TODOIST", "file:~/.todoist", "keyring:todoist"] {
            let ok = InboxAccount {
                token: good.into(),
                ..base.clone()
            };
            assert!(cfg(vec![ok]).validate().is_empty(), "{good}");
        }
    }

    #[test]
    fn base_url_is_https_or_loopback_http() {
        let base = InboxAccount {
            name: "t".into(),
            kind: InboxKind::Todoist,
            token: "env:T".into(),
            ..Default::default()
        };
        for good in [
            "https://api.todoist.com",
            "http://localhost:8080",
            "http://127.0.0.1:9",
            "http://[::1]:9",
        ] {
            let ok = InboxAccount {
                base_url: good.into(),
                ..base.clone()
            };
            assert!(cfg(vec![ok]).validate().is_empty(), "{good}");
        }
        for bad in [
            "http://api.todoist.com",
            "ftp://x",
            "not a url",
            "http://10.0.0.1",
        ] {
            let err = one_error(InboxAccount {
                base_url: bad.into(),
                ..base.clone()
            });
            assert!(err.contains("base_url"), "{bad}: {err}");
        }
        let err = one_error(InboxAccount {
            base_url: String::new(),
            ..base
        });
        assert!(err.contains("base_url: required"), "{err}");
    }

    #[test]
    fn imap_needs_host_user_password_and_a_port() {
        let base = InboxAccount {
            name: "m".into(),
            kind: InboxKind::Imap,
            ..Default::default()
        };
        let errs = cfg(vec![base.clone()]).validate();
        for key in ["host", "username", "password"] {
            assert!(
                errs.iter()
                    .any(|e| e.contains(&format!(".{key}: required"))),
                "{errs:?}"
            );
        }
        let full = InboxAccount {
            host: "imap.example.com".into(),
            username: "me".into(),
            password: "keyring:mail".into(),
            ..base
        };
        assert!(cfg(vec![full.clone()]).validate().is_empty());
        assert!(
            one_error(InboxAccount {
                port: 0,
                ..full.clone()
            })
            .contains("port")
        );
        let err = one_error(InboxAccount {
            password: "hunter2".into(),
            ..full
        });
        assert!(err.contains("password: must be a secret ref"), "{err}");
    }

    #[test]
    fn caps_are_bounded() {
        for (max_items, timeout_secs, ok) in [
            (1, 1, true),
            (500, 120, true),
            (0, 30, false),
            (501, 30, false),
            (50, 0, false),
            (50, 121, false),
        ] {
            let a = InboxAccount {
                max_items,
                timeout_secs,
                ..command("c")
            };
            assert_eq!(
                cfg(vec![a]).validate().is_empty(),
                ok,
                "{max_items}/{timeout_secs}"
            );
        }
    }

    #[test]
    fn effective_poll_inherits_and_floors() {
        let mut a = command("c");
        assert_eq!(a.effective_poll_secs(300), 300);
        assert_eq!(a.effective_poll_secs(0), MIN_POLL_SECS);
        a.poll_interval_secs = 90;
        assert_eq!(a.effective_poll_secs(300), 90);
        a.poll_interval_secs = 1;
        assert_eq!(a.effective_poll_secs(300), MIN_POLL_SECS);
    }

    #[test]
    fn implemented_enabled_skips_disabled_and_reserved() {
        let c = cfg(vec![
            command("a"),
            InboxAccount {
                enabled: false,
                ..command("b")
            },
            InboxAccount {
                kind: InboxKind::Slack,
                ..command("c")
            },
        ]);
        let names: Vec<_> = c.implemented_enabled().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["a"]);
    }

    #[test]
    fn debug_redacts_credentials() {
        let a = InboxAccount {
            name: "m".into(),
            kind: InboxKind::Imap,
            host: "imap.example.com".into(),
            username: "secret-user".into(),
            password: "env:SECRET_PASS".into(),
            token: "keyring:SECRET_TOKEN".into(),
            env: BTreeMap::from([("API_KEY".into(), "SECRET_ENV".into())]),
            ..Default::default()
        };
        let shown = format!("{a:?} {:?}", cfg(vec![a.clone()]));
        for secret in ["secret-user", "SECRET_PASS", "SECRET_TOKEN", "SECRET_ENV"] {
            assert!(!shown.contains(secret), "{secret} leaked: {shown}");
        }
        assert!(shown.contains("API_KEY"), "env keys stay visible");
        assert!(shown.contains("imap.example.com"));
        assert!(shown.contains(REDACTED));
        let empty = format!("{:?}", InboxAccount::default());
        assert!(
            !empty.contains(REDACTED),
            "unset fields are not marked: {empty}"
        );
    }

    #[test]
    fn config_digest_tracks_identity_not_secrets() {
        let a = InboxAccount {
            name: "m".into(),
            kind: InboxKind::Imap,
            host: "imap.example.com".into(),
            username: "me".into(),
            password: "env:A".into(),
            ..Default::default()
        };
        let digest = a.config_digest();
        assert_eq!(digest.len(), 32);
        assert_eq!(digest, a.clone().config_digest(), "stable");
        let rotated = InboxAccount {
            password: "env:B".into(),
            token: "env:C".into(),
            env: BTreeMap::from([("K".into(), "v".into())]),
            max_items: 7,
            timeout_secs: 9,
            poll_interval_secs: 999,
            enabled: false,
            ..a.clone()
        };
        assert_eq!(
            rotated.config_digest(),
            digest,
            "secrets and tuning do not re-baseline"
        );
        for changed in [
            InboxAccount {
                host: "other.example.com".into(),
                ..a.clone()
            },
            InboxAccount {
                port: 143,
                ..a.clone()
            },
            InboxAccount {
                username: "you".into(),
                ..a.clone()
            },
            InboxAccount {
                mailbox: "Archive".into(),
                ..a.clone()
            },
            InboxAccount {
                unseen_only: false,
                ..a.clone()
            },
            InboxAccount {
                filter: "p1".into(),
                ..a.clone()
            },
            InboxAccount {
                base_url: "https://x.example".into(),
                ..a.clone()
            },
            InboxAccount {
                cwd: "/tmp".into(),
                ..a.clone()
            },
            InboxAccount {
                command: vec!["x".into()],
                ..a.clone()
            },
            InboxAccount {
                kind: InboxKind::Todoist,
                ..a.clone()
            },
        ] {
            assert_ne!(changed.config_digest(), digest, "{changed:?}");
        }
        // Argument boundaries are part of the identity.
        let ab = InboxAccount {
            command: vec!["ab".into()],
            ..a.clone()
        };
        let a_b = InboxAccount {
            command: vec!["a".into(), "b".into()],
            ..a
        };
        assert_ne!(ab.config_digest(), a_b.config_digest());
    }

    #[test]
    fn inbox_is_in_no_overlay() {
        fn properties<T: schemars::JsonSchema>() -> Vec<String> {
            let schema = serde_json::to_value(schemars::schema_for!(T)).unwrap();
            schema["properties"]
                .as_object()
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default()
        }
        for (name, props) in [
            ("profile", properties::<crate::config::ProfileConfig>()),
            ("workspace", properties::<crate::config::WorkspaceConfig>()),
            (
                "repo file",
                properties::<crate::config_repo::RepoConfigFile>(),
            ),
        ] {
            assert!(!props.is_empty(), "{name}: schema walk broke");
            assert!(
                !props.iter().any(|p| p == "inbox"),
                "{name} overlay has [inbox]"
            );
        }
        assert!(
            properties::<crate::config::Config>()
                .iter()
                .any(|p| p == "inbox")
        );
        assert!(
            crate::config_validate::is_security_relevant_path("inbox.accounts[0].command"),
            "inbox is a security-relevant root"
        );
        assert!(
            crate::config_validate::validate_str("[inbox]\nbogus = 1\n")
                .iter()
                .any(|e| e.contains("bogus")),
            "an unknown key under [inbox] is refused"
        );
    }

    #[test]
    fn reload_class_is_restart() {
        use crate::config::reload_class::{ReloadClass, class_for};
        for key in ["inbox.enabled", "inbox.accounts"] {
            assert_eq!(
                class_for(key).map(|(c, _)| c),
                Some(ReloadClass::Restart),
                "{key}"
            );
        }
    }

    #[test]
    fn kind_spellings() {
        assert_eq!(InboxKind::from_str_any("exec"), Some(InboxKind::Command));
        assert!(
            InboxKind::from_str_validated("gmail")
                .unwrap_err()
                .contains("is reserved")
        );
        assert_eq!(InboxKind::default(), InboxKind::Command);
    }
}
