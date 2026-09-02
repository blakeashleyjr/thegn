//! The `[notifications.push]` config family — the push-to-phone delivery
//! channel and its guarded inbound command inbox.
//!
//! Kept in a sibling module (rather than the god-file `config.rs`) to keep it
//! flat; `config.rs` re-exports [`PushConfig`] / [`PushKind`] and nests it on
//! `NotificationsConfig`.
//!
//! **Two halves, both off by default:**
//!
//! - **Outbound** ([`PushConfig`]): a delivery channel behind the push-provider
//!   seam (`thegn_svc::push`). `kind = ntfy`, `webhook`, `discord`, and `slack`
//!   are implemented; `telegram` / `gotify` / `pushover` are reserved. The router
//!   (`crate::notification_route`) decides *whether* a notification pushes; this
//!   is the *where/how*. The auth token is a SecretRef (`env:` / `file:`).
//! - **Inbound** ([`PushInboxConfig`]): a phone-initiated command inbox hosted
//!   by the daemon, **hard-off by default**. Enabling it requires a SecretRef
//!   `inbox_secret` and a non-empty `allow` list; the admission ceiling is
//!   `allow ∩ required_scope ∩ unconditional-admin-deny`. See
//!   `crate::push_inbox` for the pure envelope/admission logic.

use serde::{Deserialize, Serialize};

use crate::config::{config_enum, config_warn, expand_env_ref};
use crate::notification::Priority;

config_enum! {
    /// Which push provider `[notifications.push]` delivers through. `ntfy`
    /// (self-hostable pub/sub over plain HTTP, stock mobile apps), generic
    /// webhook, Discord, and Slack are implemented; the rest are reserved —
    /// accepted by config so the seam is declared, but unimplemented in this
    /// build (they error gracefully in `thegn doctor`).
    pub enum PushKind : "push channel" {
        Ntfy     = "ntfy",
        // Reserved until their publishers land (telegram = AI 423).
        Telegram = "telegram" reserved,
        Gotify   = "gotify" reserved,
        Pushover = "pushover" reserved,
        Webhook  = "webhook",
        Discord  = "discord",
        Slack    = "slack",
    } default = Ntfy;
}

/// `[notifications.push]` — the outbound push-to-phone delivery channel.
///
/// Present-but-unconfigured is inert: a `topic`-less config publishes nothing.
/// The channel only fires for a notification the router
/// (`crate::notification_route::decide`) authorised for `push` **and** whose
/// effective priority clears [`PushConfig::min_priority`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PushConfig {
    /// The legacy scalar push provider. Named providers belong in `sinks`.
    pub kind: PushKind,
    /// The push server base URL, e.g. `"https://ntfy.sh"`. Self-hosting is
    /// strongly recommended — the server sees message plaintext.
    pub server: String,
    /// The topic to publish to. **Required** to enable outbound push; treat it
    /// as a capability URL (anyone who knows it can read your notifications on a
    /// public server). Empty ⇒ outbound push is off.
    pub topic: String,
    /// Auth token for a protected topic. A SecretRef — `"env:NTFY_TOKEN"` /
    /// `"file:~/.config/thegn/ntfy-token"` — never a raw token in config. Empty
    /// ⇒ no `Authorization` header (a public topic).
    pub token: String,
    /// The channel priority floor: notifications whose effective priority is
    /// below this never push (they still record in the inbox). One of
    /// `"info"` / `"notice"` / `"alert"`. Default `"notice"`.
    pub min_priority: String,
    /// Explicit named push sinks. When empty, the legacy scalar fields above
    /// materialize one sink named after `kind`.
    pub sinks: Vec<PushSinkConfig>,
    /// The inbound command inbox (hard-off by default). See [`PushInboxConfig`].
    pub inbox: PushInboxConfig,
}

impl Default for PushConfig {
    fn default() -> Self {
        PushConfig {
            kind: PushKind::Ntfy,
            server: "https://ntfy.sh".into(),
            topic: String::new(),
            token: String::new(),
            min_priority: "notice".into(),
            sinks: Vec::new(),
            inbox: PushInboxConfig::default(),
        }
    }
}

impl PushConfig {
    /// True when outbound push is usable: an implemented kind with a server and
    /// a topic. A reserved kind or a missing topic ⇒ inert (nothing published).
    pub fn is_configured(&self) -> bool {
        self.effective_sinks()
            .iter()
            .any(PushSinkConfig::is_configured)
    }

    /// The resolved auth token (SecretRef expanded), or `None` for a public
    /// topic. Never returns an empty string.
    pub fn resolved_token(&self) -> Option<String> {
        expand_env_ref(&self.token)
    }

    /// The parsed channel priority floor; garbage falls back to `Notice`.
    pub fn min_priority(&self) -> Priority {
        Priority::parse(&self.min_priority).unwrap_or(Priority::Notice)
    }

    /// Materialize the backward-compatible scalar form into a named sink.
    /// Explicit sinks are returned in config order, which is also the stable
    /// fan-out order used by the router.
    pub fn effective_sinks(&self) -> Vec<PushSinkConfig> {
        if self.sinks.is_empty() {
            vec![PushSinkConfig {
                name: self.kind.as_str().to_string(),
                kind: self.kind,
                server: self.server.clone(),
                topic: self.topic.clone(),
                token: self.token.clone(),
                url: String::new(),
                min_priority: self.min_priority.clone(),
            }]
        } else {
            self.sinks.clone()
        }
    }

    /// Static validation for named sinks and webhook URL custody. Errors never
    /// include endpoint text because webhook URLs are bearer credentials.
    pub fn validate_errors(&self) -> Vec<String> {
        use crate::seam::Kind;
        use std::collections::BTreeSet;

        const MAX_SINKS: usize = 32;
        const MAX_NAME_CHARS: usize = 64;
        let sinks = self.effective_sinks();
        let mut errors = Vec::new();
        if self.sinks.len() > MAX_SINKS {
            errors.push(format!(
                "[notifications.push.sinks] more than {MAX_SINKS} sinks"
            ));
        }
        let mut names = BTreeSet::new();
        for sink in &sinks {
            let name = sink.name.trim();
            if name.is_empty() {
                errors.push("[notifications.push.sinks] sink name must not be empty".to_string());
            } else if name != sink.name {
                errors.push(format!(
                    "[notifications.push.sinks] sink name {name:?} must not have leading or trailing whitespace"
                ));
            } else if name.chars().any(char::is_control) {
                errors.push(format!(
                    "[notifications.push.sinks] sink name {name:?} must not contain control characters"
                ));
            } else if name.chars().count() > MAX_NAME_CHARS {
                errors.push(format!(
                    "[notifications.push.sinks] sink {name:?} name exceeds {MAX_NAME_CHARS} characters"
                ));
            } else if !names.insert(name.to_string()) {
                errors.push(format!(
                    "[notifications.push.sinks] duplicate sink name {name:?}"
                ));
            }
            if !matches!(
                sink.kind,
                PushKind::Ntfy | PushKind::Webhook | PushKind::Discord | PushKind::Slack
            ) && sink.kind.is_reserved()
            {
                // The enum walker reports the kind itself; this names the sink
                // for the multi-sink form without duplicating the enum error.
                errors.push(format!(
                    "[notifications.push.sinks] sink {name:?} selects reserved kind {}",
                    sink.kind.as_str()
                ));
            }
            if Priority::parse(&sink.min_priority).is_none() {
                errors.push(format!(
                    "[notifications.push.sinks] sink {name:?} has invalid min_priority (expected info, notice, or alert)"
                ));
            }
            if matches!(
                sink.kind,
                PushKind::Webhook | PushKind::Discord | PushKind::Slack
            ) {
                let url = sink.url.trim();
                let valid_ref = ["env:", "file:"].iter().any(|prefix| {
                    url.strip_prefix(prefix)
                        .is_some_and(|operand| !operand.trim().is_empty())
                });
                if !valid_ref {
                    errors.push(format!(
                        "[notifications.push.sinks] sink {name:?} url must be a SecretRef (env:VAR or file:PATH), never a raw URL"
                    ));
                }
            }
        }
        for (index, sink) in self.sinks.iter().enumerate() {
            if sink.name.trim().is_empty() {
                errors.push(format!(
                    "[notifications.push.sinks[{index}]] name is required"
                ));
            }
        }
        errors
    }
}

/// One explicitly named sink under `[[notifications.push.sinks]]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PushSinkConfig {
    /// Stable selector used by `push:<name>` rules and delivery diagnostics.
    pub name: String,
    /// Provider kind for this sink.
    pub kind: PushKind,
    /// ntfy server base URL.
    pub server: String,
    /// ntfy topic.
    pub topic: String,
    /// ntfy auth token (legacy-compatible field).
    pub token: String,
    /// Webhook/Discord/Slack endpoint, accepted only as `env:` or `file:`.
    pub url: String,
    /// Per-sink priority floor.
    pub min_priority: String,
}

impl Default for PushSinkConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: PushKind::Ntfy,
            server: "https://ntfy.sh".into(),
            topic: String::new(),
            token: String::new(),
            url: String::new(),
            min_priority: "notice".into(),
        }
    }
}

impl PushSinkConfig {
    pub fn is_configured(&self) -> bool {
        use crate::seam::Kind;
        if self.kind.is_reserved() {
            return false;
        }
        match self.kind {
            PushKind::Ntfy => !self.server.trim().is_empty() && !self.topic.trim().is_empty(),
            PushKind::Webhook | PushKind::Discord | PushKind::Slack => !self.url.trim().is_empty(),
            PushKind::Telegram | PushKind::Gotify | PushKind::Pushover => false,
        }
    }

    pub fn min_priority(&self) -> Priority {
        Priority::parse(&self.min_priority).unwrap_or(Priority::Notice)
    }
}

/// `[notifications.push.inbox]` — the guarded, phone-initiated command inbox.
///
/// **Hard-off by default.** Enabling it (`enabled = true`) is a *startup
/// configuration error* unless it also carries a SecretRef `inbox_secret` and a
/// non-empty `allow` list — a subscribed-but-inert inbox is not a valid state.
/// Every accepted message is a signed envelope mapping to exactly one catalog
/// capability, admitted only by `allow ∩ scopes ∩ unconditional-admin-deny`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(default)]
pub struct PushInboxConfig {
    /// Master switch. `false` (default) ⇒ the daemon opens no command
    /// subscription and no phone-initiated command path exists.
    pub enabled: bool,
    /// The command topic (separate from the outbound `[notifications.push]`
    /// topic). Required when enabled.
    pub topic: String,
    /// The HMAC secret every envelope is signed with. A SecretRef
    /// (`"env:THEGN_INBOX_SECRET"` / `"file:…"`) — **required** when enabled,
    /// and a raw literal is rejected (a topic is guessable; the MAC is the real
    /// authenticator, so its key must live outside the config file).
    pub inbox_secret: String,
    /// The capability ids callable from the inbox (e.g. `"worktree.list"` →
    /// `"worktrees.list"`). Empty ⇒ nothing callable, which is a config error
    /// when enabled (enable-and-inert is not a state).
    pub allow: Vec<String>,
    /// The scope ceiling. An allowed capability still executes only when its
    /// `required_scope` is within this set. Default `["read"]`; admin-scoped
    /// capabilities are refused unconditionally regardless of this.
    pub scopes: Vec<String>,
    /// Optional topic to publish truncated command results to. Empty ⇒ replies
    /// are not published (fire-and-forget).
    pub reply_topic: String,
}

impl Default for PushInboxConfig {
    fn default() -> Self {
        PushInboxConfig {
            enabled: false,
            topic: String::new(),
            inbox_secret: String::new(),
            allow: Vec::new(),
            scopes: vec!["read".into()],
            reply_topic: String::new(),
        }
    }
}

impl PushInboxConfig {
    /// Whether `inbox_secret` is a SecretRef (`env:` / `file:`) rather than a
    /// raw literal. Raw tokens in config are rejected — the whole point of the
    /// MAC is a key the config file never contains.
    pub fn secret_is_ref(&self) -> bool {
        let s = self.inbox_secret.trim();
        s.starts_with("env:") || s.starts_with("file:")
    }

    /// The resolved HMAC secret (SecretRef expanded), or `None` when unset /
    /// unresolvable. Never returns an empty string.
    pub fn resolved_secret(&self) -> Option<String> {
        if !self.secret_is_ref() {
            return None;
        }
        expand_env_ref(&self.inbox_secret)
    }

    /// The scope ceiling as a [`crate::control::ScopeSet`].
    pub fn ceiling(&self) -> crate::control::ScopeSet {
        crate::control::ScopeSet::parse(&self.scopes.join(","))
    }

    /// The allow list as a set of capability ids.
    pub fn allow_set(&self) -> std::collections::BTreeSet<String> {
        self.allow
            .iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Static (env-independent) configuration errors, each naming the offending
    /// key. Used by `thegn config validate` and, with resolution added, by the
    /// daemon at inbox-start (see [`Self::startup_block_reason`]). An empty list
    /// means the *shape* is valid; the secret may still fail to resolve at
    /// runtime, which is a separate, environment-dependent check.
    pub fn validate_errors(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if !self.enabled {
            return errs;
        }
        if self.topic.trim().is_empty() {
            errs.push("[notifications.push.inbox] topic is required when enabled = true".into());
        }
        if self.inbox_secret.trim().is_empty() {
            errs.push(
                "[notifications.push.inbox] inbox_secret is required when enabled = true \
                 (use a secret ref: env:VAR or file:PATH)"
                    .into(),
            );
        } else if !self.secret_is_ref() {
            errs.push(
                "[notifications.push.inbox] inbox_secret must be a secret ref \
                 (env:VAR or file:PATH), never a raw token in config"
                    .into(),
            );
        }
        if self.allow_set().is_empty() {
            errs.push(
                "[notifications.push.inbox] allow must list at least one capability when \
                 enabled = true (an empty allow list is subscribed-but-inert)"
                    .into(),
            );
        }
        for cap in self.allow_set() {
            match crate::capability::lookup(&cap) {
                None => errs.push(format!(
                    "[notifications.push.inbox] allow lists unknown capability {cap:?} \
                     (see `thegn api list`)"
                )),
                Some(c) if crate::capability::scope_of(c) == crate::control::Scope::Admin => errs
                    .push(format!(
                        "[notifications.push.inbox] allow lists admin-scoped capability {cap:?}, \
                         which the inbox refuses unconditionally — remove it"
                    )),
                Some(_) => {}
            }
        }
        for word in &self.scopes {
            let w = word.trim().to_ascii_lowercase();
            if !w.is_empty() && !matches!(w.as_str(), "read" | "write" | "git" | "admin") {
                errs.push(format!(
                    "[notifications.push.inbox] scopes has unknown scope {word:?} \
                     (expected: read, write, git, admin)"
                ));
            }
        }
        errs
    }

    /// The reason the daemon must NOT start the inbox, or `None` when it may.
    /// Combines the static shape checks with the runtime secret resolution (the
    /// SecretRef must resolve to a non-empty value in *this* environment).
    /// Returns the first problem so the daemon log names one actionable fix.
    pub fn startup_block_reason(&self) -> Option<String> {
        if let Some(e) = self.validate_errors().into_iter().next() {
            return Some(e);
        }
        if self.enabled && self.resolved_secret().is_none() {
            return Some(
                "[notifications.push.inbox] inbox_secret does not resolve to a value in this \
                 environment (env var unset or file unreadable/empty)"
                    .into(),
            );
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::Scope;
    use crate::seam::Kind;

    #[test]
    fn push_kind_parses_and_reserves() {
        assert_eq!(PushKind::from_str_validated("ntfy"), Ok(PushKind::Ntfy));
        assert!(!PushKind::Ntfy.is_reserved());
        for reserved in ["telegram", "gotify", "pushover"] {
            let e = PushKind::from_str_validated(reserved).unwrap_err();
            assert!(e.contains(reserved) && e.contains("reserved"), "{e}");
        }
        assert!(PushKind::Telegram.is_reserved());
        assert!(!PushKind::Webhook.is_reserved());
        assert!(!PushKind::Discord.is_reserved());
        assert!(!PushKind::Slack.is_reserved());
        assert!(PushKind::from_str_validated("matrix").is_err());
    }

    #[test]
    fn is_configured_requires_impl_kind_server_topic() {
        let mut c = PushConfig::default();
        assert!(!c.is_configured(), "default has no topic");
        c.topic = "thegn-alerts".into();
        assert!(c.is_configured());
        c.server = "  ".into();
        assert!(!c.is_configured(), "blank server");
        c.server = "https://ntfy.sh".into();
        c.kind = PushKind::Telegram; // reserved
        assert!(!c.is_configured(), "reserved kind is inert");
    }

    #[test]
    fn min_priority_parses_with_fallback() {
        let mut c = PushConfig::default();
        assert_eq!(c.min_priority(), Priority::Notice);
        c.min_priority = "alert".into();
        assert_eq!(c.min_priority(), Priority::Alert);
        c.min_priority = "garbage".into();
        assert_eq!(c.min_priority(), Priority::Notice, "fallback");
    }

    #[test]
    fn explicit_sinks_materialize_and_validate_without_echoing_urls() {
        let mut c = PushConfig::default();
        c.sinks = vec![PushSinkConfig {
            name: "oncall".into(),
            kind: PushKind::Slack,
            url: "https://hooks.slack.test/SECRET_SENTINEL".into(),
            ..Default::default()
        }];
        assert_eq!(c.effective_sinks().len(), 1);
        assert!(c.is_configured());
        let errors = c.validate_errors();
        assert!(errors.iter().any(|e| e.contains("oncall")));
        assert!(!errors.iter().any(|e| e.contains("SECRET_SENTINEL")));
    }

    #[test]
    fn named_sink_secret_refs_and_floors_are_accepted() {
        let c = PushConfig {
            sinks: vec![
                PushSinkConfig {
                    name: "phone".into(),
                    kind: PushKind::Ntfy,
                    topic: "alerts".into(),
                    min_priority: "alert".into(),
                    ..Default::default()
                },
                PushSinkConfig {
                    name: "oncall".into(),
                    kind: PushKind::Discord,
                    url: "env:DISCORD_WEBHOOK".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert!(c.validate_errors().is_empty(), "{:?}", c.validate_errors());
        assert_eq!(c.effective_sinks()[0].min_priority(), Priority::Alert);
    }

    #[test]
    fn sink_names_cannot_inject_output_or_break_selectors() {
        let mut c = PushConfig {
            sinks: vec![PushSinkConfig {
                name: " oncall".into(),
                topic: "alerts".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let errors = c.validate_errors();
        assert!(errors.iter().any(|e| e.contains("leading or trailing")));

        c.sinks[0].name = "on\u{1b}[31mcall".into();
        let errors = c.validate_errors();
        assert!(errors.iter().any(|e| e.contains("control characters")));
    }

    #[test]
    fn token_resolves_as_secret_ref() {
        let mut c = PushConfig::default();
        assert_eq!(c.resolved_token(), None, "empty");
        c.token = "a-literal".into();
        // expand_env_ref returns a bare literal as-is (the outbound token
        // follows the lenient convention; the inbox secret does not).
        assert_eq!(c.resolved_token().as_deref(), Some("a-literal"));
    }

    #[test]
    fn disabled_inbox_has_no_errors() {
        let inbox = PushInboxConfig::default();
        assert!(inbox.validate_errors().is_empty());
        assert!(inbox.startup_block_reason().is_none());
    }

    #[test]
    fn enabled_inbox_requires_secret_and_allow() {
        let inbox = PushInboxConfig {
            enabled: true,
            topic: "cmd".into(),
            ..Default::default()
        };
        let errs = inbox.validate_errors();
        // Missing secret AND empty allow ⇒ two named errors.
        assert!(errs.iter().any(|e| e.contains("inbox_secret")), "{errs:?}");
        assert!(errs.iter().any(|e| e.contains("allow")), "{errs:?}");
    }

    #[test]
    fn enabled_inbox_rejects_raw_secret() {
        let inbox = PushInboxConfig {
            enabled: true,
            topic: "cmd".into(),
            inbox_secret: "s3cr3t-in-the-clear".into(),
            allow: vec!["worktrees.list".into()],
            ..Default::default()
        };
        let errs = inbox.validate_errors();
        assert!(
            errs.iter()
                .any(|e| e.contains("inbox_secret") && e.contains("secret ref")),
            "raw token must be rejected: {errs:?}"
        );
    }

    #[test]
    fn allow_rejects_unknown_and_admin_caps() {
        let inbox = PushInboxConfig {
            enabled: true,
            topic: "cmd".into(),
            inbox_secret: "env:X".into(),
            allow: vec![
                "worktrees.list".into(),
                "does.not.exist".into(),
                "daemon.shutdown".into(),
            ],
            ..Default::default()
        };
        let errs = inbox.validate_errors();
        assert!(
            errs.iter().any(|e| e.contains("does.not.exist")),
            "{errs:?}"
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("daemon.shutdown") && e.contains("admin")),
            "admin cap must be rejected: {errs:?}"
        );
        // daemon.shutdown is indeed admin-scoped (sanity on the fixture).
        assert_eq!(
            crate::capability::scope_of(crate::capability::lookup("daemon.shutdown").unwrap()),
            Scope::Admin
        );
    }

    #[test]
    fn well_formed_inbox_shape_is_valid_but_secret_may_not_resolve() {
        let inbox = PushInboxConfig {
            enabled: true,
            topic: "cmd".into(),
            inbox_secret: "env:THEGN_INBOX_SECRET_DEFINITELY_UNSET_XYZ".into(),
            allow: vec!["worktrees.list".into(), "git.status".into()],
            scopes: vec!["read".into()],
            ..Default::default()
        };
        assert!(inbox.validate_errors().is_empty(), "shape is valid");
        // But the env var is unset here, so the daemon must refuse to start it.
        let reason = inbox.startup_block_reason().unwrap();
        assert!(reason.contains("resolve"), "{reason}");
    }

    #[test]
    fn ceiling_and_allow_set() {
        let inbox = PushInboxConfig {
            scopes: vec!["read".into(), "git".into()],
            allow: vec!["git.status".into(), " ".into(), "pr.status".into()],
            ..Default::default()
        };
        let c = inbox.ceiling();
        assert!(c.allows(Scope::Read) && c.allows(Scope::Git));
        assert!(!c.allows(Scope::Write));
        let set = inbox.allow_set();
        assert_eq!(set.len(), 2, "blank entry dropped: {set:?}");
    }

    #[test]
    fn resolved_secret_reads_a_file_ref() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox-secret");
        std::fs::write(&path, "hunter2\n").unwrap();
        let inbox = PushInboxConfig {
            enabled: true,
            topic: "cmd".into(),
            inbox_secret: format!("file:{}", path.display()),
            allow: vec!["worktrees.list".into()],
            ..Default::default()
        };
        assert!(inbox.secret_is_ref());
        assert_eq!(inbox.resolved_secret().as_deref(), Some("hunter2"));
        // A validly-shaped, resolvable inbox is startable.
        assert!(inbox.validate_errors().is_empty());
        assert!(inbox.startup_block_reason().is_none());
    }

    #[test]
    fn bad_scope_word_is_an_error() {
        let inbox = PushInboxConfig {
            enabled: true,
            topic: "cmd".into(),
            inbox_secret: "env:X".into(),
            allow: vec!["worktrees.list".into()],
            scopes: vec!["read".into(), "superuser".into()],
            ..Default::default()
        };
        let errs = inbox.validate_errors();
        assert!(errs.iter().any(|e| e.contains("superuser")), "{errs:?}");
    }
}
