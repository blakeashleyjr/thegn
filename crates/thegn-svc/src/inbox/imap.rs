//! The read-only IMAP inbox backend (b163, THE-2216).
//!
//! Implicit TLS only (port 993 by default; STARTTLS is not offered), rustls
//! over the ring provider with the webpki root set, SNI = the configured host.
//! Outside tests there is no plaintext path at all: the loopback transport
//! seam is a `cfg(test)` field.
//!
//! One poll is: `CAPABILITY`; `AUTHENTICATE PLAIN` (SASL-IR) or `LOGIN`;
//! `EXAMINE` (read-only, never `SELECT`); `UID SEARCH`; `UID FETCH` of the
//! newest UIDs with `BODY.PEEK` (never sets `\Seen`); `LOGOUT`. No command
//! that changes a mailbox (`STORE`, `EXPUNGE`, `COPY`, `MOVE`, `SELECT`) can be
//! built by [`super::imap_proto`], so none can be sent.
//!
//! The password is resolved at fetch time through the secret broker and kept
//! in zeroed-on-drop buffers. No error string carries it, the login name, or
//! server free text: errors name the command and at most the server's
//! response-code word.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use thegn_core::config::InboxAccount;
use thegn_core::config_inbox::MAX_TIMEOUT_SECS;
use thegn_core::inbox::{InboxItem, InboxPage, PageMode};
use thegn_core::seam::{Availability, ProbeReport};
use thegn_core::secretref::{BareAs, SecretRef};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

use super::imap_proto::{self as proto, ProtoError, Response, Status, StatusKind, Tag};
use super::{InboxBackend, InboxCaps, InboxError};

/// The default implicit-TLS port.
pub const DEFAULT_PORT: u16 = 993;
/// The Gmail IMAP host; its items get a web link by Message-ID.
const GMAIL_HOST: &str = "imap.gmail.com";
/// Bytes read from the socket per call.
const READ_CHUNK: usize = 8 * 1024;
/// How far back `UID SEARCH SINCE` looks when `unseen_only = false`.
const SINCE_DAYS: u64 = 7;

/// The byte stream one session runs over (TLS in production).
pub(crate) trait ImapStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> ImapStream for T {}

/// The test-only transport seam: opens a stream to `(host, port)`.
#[cfg(test)]
pub(crate) type Connector =
    fn(String, u16) -> BoxFuture<'static, Result<Box<dyn ImapStream>, InboxError>>;

/// One `kind = "imap"` account.
pub struct ImapBackend {
    account: String,
    host: String,
    port: u16,
    username: String,
    password_ref: String,
    mailbox: String,
    unseen_only: bool,
    timeout: Duration,
    /// Set only by tests: a plain loopback transport instead of TLS.
    #[cfg(test)]
    connect: Option<Connector>,
}

impl ImapBackend {
    /// The backend for `account`. `NotConfigured` when host, username or
    /// password is missing.
    pub fn from_account(account: &InboxAccount) -> Result<Self, InboxError> {
        if account.host.trim().is_empty()
            || account.username.is_empty()
            || account.password.trim().is_empty()
        {
            return Err(InboxError::NotConfigured);
        }
        let mailbox = if account.mailbox.is_empty() {
            "INBOX".to_owned()
        } else {
            account.mailbox.clone()
        };
        Ok(ImapBackend {
            account: account.name.clone(),
            host: account.host.trim().to_owned(),
            port: if account.port == 0 {
                DEFAULT_PORT
            } else {
                account.port
            },
            username: account.username.clone(),
            password_ref: account.password.clone(),
            mailbox,
            unseen_only: account.unseen_only,
            timeout: Duration::from_secs(account.timeout_secs.clamp(1, MAX_TIMEOUT_SECS)),
            #[cfg(test)]
            connect: None,
        })
    }

    /// Resolve the password ref. A literal, unset or unresolvable ref is
    /// `Auth` (never echoing the value).
    fn password(&self) -> Result<Zeroizing<String>, InboxError> {
        let reference = SecretRef::parse(&self.password_ref, BareAs::Literal);
        if reference.is_literal() {
            return Err(InboxError::Auth(
                "password must be a secret ref (keyring:/env:/file:)".into(),
            ));
        }
        if !reference.is_configured() {
            return Err(InboxError::Auth("no password configured".into()));
        }
        let consumer = format!("inbox:{}", self.account);
        match crate::secret::resolve_ref(&reference, &consumer) {
            Some(value) if !value.is_empty() => Ok(Zeroizing::new(value)),
            Some(_) => Err(InboxError::Auth("password secret ref is empty".into())),
            None => Err(InboxError::Auth(
                "password secret ref did not resolve".into(),
            )),
        }
    }

    async fn open(&self) -> Result<Box<dyn ImapStream>, InboxError> {
        #[cfg(test)]
        {
            if let Some(connect) = self.connect {
                return connect(self.host.clone(), self.port).await;
            }
        }
        connect_tls(&self.host, self.port).await
    }

    async fn run(&self, password: &str, max_items: usize) -> Result<InboxPage, InboxError> {
        let stream = self.open().await?;
        let mut session = Session::new(stream);
        let page = self.converse(&mut session, password, max_items).await;
        session.shutdown().await;
        page
    }

    async fn converse(
        &self,
        s: &mut Session,
        password: &str,
        max_items: usize,
    ) -> Result<InboxPage, InboxError> {
        let mut budget = 0usize;
        let greeting = s.read_response(&mut budget).await?;
        let preauth = match (&greeting.tag, proto::status(&greeting)) {
            (
                Tag::Untagged,
                Some(Status {
                    kind: StatusKind::Ok,
                    ..
                }),
            ) => false,
            (
                Tag::Untagged,
                Some(Status {
                    kind: StatusKind::Preauth,
                    ..
                }),
            ) => true,
            (
                Tag::Untagged,
                Some(Status {
                    kind: StatusKind::Bye,
                    ..
                }),
            ) => {
                return Err(InboxError::Network(
                    "server refused the connection (BYE)".into(),
                ));
            }
            _ => return Err(InboxError::Parse("unexpected IMAP greeting".into())),
        };

        // 1. CAPABILITY
        let reply = s
            .command("CAPABILITY", |t| Ok(proto::cmd_capability(t)))
            .await?;
        reply.require_ok("CAPABILITY")?;
        let caps: BTreeSet<String> = reply
            .untagged
            .iter()
            .filter_map(proto::capabilities)
            .flatten()
            .collect();

        // 2. login
        if !preauth {
            self.login(s, &caps, password).await?;
        }

        // 3. EXAMINE (read-only)
        let reply = s
            .command("EXAMINE", |t| proto::cmd_examine(t, &self.mailbox))
            .await?;
        reply.require_ok("EXAMINE")?;
        if reply
            .status
            .code_word()
            .is_some_and(|code| code == "READ-WRITE")
        {
            return Err(InboxError::Api(
                "EXAMINE opened the mailbox read-write".into(),
            ));
        }
        let uidvalidity = reply
            .untagged
            .iter()
            .find_map(proto::uidvalidity)
            .ok_or_else(|| InboxError::Parse("EXAMINE returned no UIDVALIDITY".into()))?;

        // 4. UID SEARCH
        let since = chrono::Utc::now().date_naive();
        let since = since
            .checked_sub_days(chrono::Days::new(SINCE_DAYS))
            .unwrap_or(since);
        let unseen_only = self.unseen_only;
        let reply = s
            .command("SEARCH", |t| {
                Ok(if unseen_only {
                    proto::cmd_uid_search_unseen(t)
                } else {
                    proto::cmd_uid_search_since(t, since)
                })
            })
            .await?;
        reply.require_ok("SEARCH")?;
        let mut uids = Vec::new();
        for response in &reply.untagged {
            if let Some(found) = proto::search(response) {
                uids.extend(found.map_err(proto_error)?);
            }
        }
        uids.sort_unstable();
        uids.dedup();

        // 5. the newest max_items
        let take = max_items.min(proto::MAX_UIDS);
        let truncated = uids.len() > take;
        let wanted: Vec<u32> = uids[uids.len() - take.min(uids.len())..].to_vec();

        // 6. UID FETCH (BODY.PEEK)
        let mut items = Vec::new();
        if !wanted.is_empty() {
            let reply = s
                .command("FETCH", |t| proto::cmd_uid_fetch(t, &wanted))
                .await?;
            reply.require_ok("FETCH")?;
            let wanted: BTreeSet<u32> = wanted.iter().copied().collect();
            let mut by_uid = BTreeMap::new();
            for response in &reply.untagged {
                let Some(parsed) = proto::fetch(response) else {
                    continue;
                };
                let parsed = parsed.map_err(proto_error)?;
                let (Some(uid), Some(header)) = (parsed.uid, parsed.header.as_deref()) else {
                    // An unsolicited flag update, not our answer.
                    continue;
                };
                if !wanted.contains(&uid) {
                    continue;
                }
                by_uid.insert(
                    uid,
                    self.item(uidvalidity, uid, header, parsed.internal_date),
                );
            }
            items = by_uid.into_values().rev().collect();
        }

        // 7. LOGOUT
        s.logout().await;

        Ok(InboxPage {
            items,
            removed: Vec::new(),
            cursor: Some(uidvalidity.to_string()),
            mode: PageMode::Snapshot,
            truncated,
        })
    }

    async fn login(
        &self,
        s: &mut Session,
        caps: &BTreeSet<String>,
        password: &str,
    ) -> Result<(), InboxError> {
        if caps.contains("LOGINDISABLED") {
            return Err(InboxError::Auth(
                "server advertises LOGINDISABLED; credentials not sent".into(),
            ));
        }
        let user = self.username.as_str();
        let plain = caps.contains("AUTH=PLAIN") && caps.contains("SASL-IR");
        let (name, sent) = if plain {
            (
                "AUTHENTICATE",
                s.command("AUTHENTICATE", |t| {
                    proto::cmd_authenticate_plain(t, user, password)
                })
                .await,
            )
        } else {
            (
                "LOGIN",
                s.command("LOGIN", |t| proto::cmd_login(t, user, password))
                    .await,
            )
        };
        let reply = sent.map_err(|e| match e {
            InboxError::Parse(_) | InboxError::Api(_) => {
                InboxError::Auth(format!("{name} failed: unusable credentials or reply"))
            }
            other => other,
        })?;
        if reply.status.kind != StatusKind::Ok {
            return Err(InboxError::Auth(format!(
                "{name} rejected{}",
                code_suffix(&reply.status)
            )));
        }
        Ok(())
    }

    fn item(
        &self,
        uidvalidity: u32,
        uid: u32,
        header: &[u8],
        updated_at: Option<i64>,
    ) -> InboxItem {
        let headers = proto::parse_headers(header);
        let title = headers
            .subject
            .as_deref()
            .map(proto::decode_words)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "(no subject)".to_owned());
        let from = headers
            .from
            .as_deref()
            .map(proto::sender_display)
            .unwrap_or_default();
        let gmail = self
            .host
            .trim_end_matches('.')
            .eq_ignore_ascii_case(GMAIL_HOST);
        let url = match headers.message_id.as_deref() {
            Some(id) if gmail && !id.is_empty() => format!(
                "https://mail.google.com/mail/u/0/#search/rfc822msgid:{}",
                proto::percent_encode(id)
            ),
            _ => String::new(),
        };
        InboxItem {
            id: format!("{uidvalidity}-{uid}"),
            title,
            url,
            from,
            labels: Vec::new(),
            due: None,
            updated_at,
            priority: None,
        }
    }
}

impl InboxBackend for ImapBackend {
    fn kind(&self) -> &'static str {
        "imap"
    }

    fn caps(&self) -> InboxCaps {
        InboxCaps::default()
    }

    fn fetch<'a>(
        &'a self,
        _cursor: &'a str,
        max_items: usize,
    ) -> BoxFuture<'a, Result<InboxPage, InboxError>> {
        Box::pin(async move {
            let password = self.password()?;
            tokio::time::timeout(self.timeout, self.run(&password, max_items))
                .await
                .unwrap_or(Err(InboxError::Timeout))
        })
    }
}

/// The offline probe: Ready when host, username and a password ref are set.
pub(crate) fn probe(account: &InboxAccount, id: String) -> ProbeReport {
    let reference = SecretRef::parse(&account.password, BareAs::Literal);
    let availability = if account.host.trim().is_empty() {
        Availability::Unavailable("no host configured".into())
    } else if account.username.is_empty() {
        Availability::Unavailable("no username configured".into())
    } else if !reference.is_configured() {
        Availability::Unavailable("no password configured".into())
    } else if reference.is_literal() {
        Availability::Unavailable("password must be a secret ref (keyring:/env:/file:)".into())
    } else {
        Availability::Ready
    };
    ProbeReport::new("inbox", id, availability).note("read-only (EXAMINE, BODY.PEEK)")
}

/// Implicit TLS to `host:port`: rustls (ring provider), webpki roots, SNI =
/// host. Certificate verification is never disabled.
async fn connect_tls(host: &str, port: u16) -> Result<Box<dyn ImapStream>, InboxError> {
    use tokio_rustls::rustls;
    let name = rustls::pki_types::ServerName::try_from(host)
        .map(|name| name.to_owned())
        .map_err(|_| InboxError::Network("invalid TLS server name".into()))?;
    let roots: rustls::RootCertStore = webpki_roots::TLS_SERVER_ROOTS.iter().cloned().collect();
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| InboxError::Network(format!("TLS setup: {e}")))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|e| InboxError::Network(format!("connect: {}", e.kind())))?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .map_err(|e| InboxError::Network(format!("TLS handshake: {e}")))?;
    Ok(Box::new(tls))
}

fn proto_error(e: ProtoError) -> InboxError {
    InboxError::Parse(e.to_string())
}

fn code_suffix(status: &Status) -> String {
    status
        .code_word()
        .map(|code| format!(" [{code}]"))
        .unwrap_or_default()
}

/// One command's answer: its untagged responses and the tagged completion.
struct Reply {
    untagged: Vec<Response>,
    status: Status,
}

impl Reply {
    fn require_ok(&self, name: &'static str) -> Result<(), InboxError> {
        if self.status.kind == StatusKind::Ok {
            Ok(())
        } else {
            Err(InboxError::Api(format!(
                "{name} failed{}",
                code_suffix(&self.status)
            )))
        }
    }
}

/// One connection: the stream, unread bytes, and the tag counter.
struct Session {
    stream: Box<dyn ImapStream>,
    buf: Vec<u8>,
    next_tag: u32,
}

impl Session {
    fn new(stream: Box<dyn ImapStream>) -> Self {
        Session {
            stream,
            buf: Vec::new(),
            next_tag: 0,
        }
    }

    /// Read one complete response. `budget` counts the bytes read for the
    /// current command; past [`proto::MAX_RESPONSE`] it is a `BodyLimit`.
    async fn read_response(&mut self, budget: &mut usize) -> Result<Response, InboxError> {
        loop {
            if let Some((response, used)) = proto::parse_response(&self.buf).map_err(proto_error)? {
                self.buf.drain(..used);
                return Ok(response);
            }
            let mut chunk = [0u8; READ_CHUNK];
            let n = self
                .stream
                .read(&mut chunk)
                .await
                .map_err(|e| InboxError::Network(format!("read: {}", e.kind())))?;
            if n == 0 {
                return Err(InboxError::Network("server closed the connection".into()));
            }
            *budget = budget.saturating_add(n);
            if *budget > proto::MAX_RESPONSE {
                return Err(proto_error(ProtoError::BodyLimit("response")));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Send one command and collect its reply. `build` gets the tag.
    async fn command(
        &mut self,
        name: &'static str,
        build: impl FnOnce(&str) -> Result<Zeroizing<Vec<u8>>, ProtoError>,
    ) -> Result<Reply, InboxError> {
        self.next_tag += 1;
        let tag = format!("A{}", self.next_tag);
        let line = build(&tag).map_err(proto_error)?;
        self.stream
            .write_all(&line)
            .await
            .map_err(|e| InboxError::Network(format!("write: {}", e.kind())))?;
        drop(line);
        self.stream
            .flush()
            .await
            .map_err(|e| InboxError::Network(format!("write: {}", e.kind())))?;
        let mut budget = self.buf.len();
        let mut untagged = Vec::new();
        loop {
            let response = self.read_response(&mut budget).await?;
            match &response.tag {
                Tag::Tagged(t) if *t == tag => {
                    let status = proto::status(&response).ok_or_else(|| {
                        InboxError::Parse(format!("{name}: completion without a status"))
                    })?;
                    return Ok(Reply { untagged, status });
                }
                Tag::Tagged(_) => {
                    return Err(InboxError::Parse(format!("{name}: unexpected tag")));
                }
                Tag::Continuation => {
                    return Err(InboxError::Api(format!(
                        "{name}: unexpected continuation request"
                    )));
                }
                Tag::Untagged => {
                    let bye = matches!(
                        proto::status(&response),
                        Some(Status {
                            kind: StatusKind::Bye,
                            ..
                        })
                    );
                    if bye && name != "LOGOUT" {
                        return Err(InboxError::Network(format!(
                            "server closed the session during {name}"
                        )));
                    }
                    if untagged.len() >= proto::MAX_UNTAGGED {
                        return Err(proto_error(ProtoError::BodyLimit("responses")));
                    }
                    untagged.push(response);
                }
            }
        }
    }

    /// Best-effort `LOGOUT`; its outcome changes nothing.
    async fn logout(&mut self) {
        if self
            .command("LOGOUT", |t| Ok(proto::cmd_logout(t)))
            .await
            .is_err()
        {
            // The server may close before the tagged OK; nothing to do.
        }
    }

    /// Best-effort close (TLS close_notify).
    async fn shutdown(&mut self) {
        if self.stream.shutdown().await.is_err() {
            // Already closed.
        }
    }
}

#[cfg(test)]
#[path = "imap_tests.rs"]
mod tests;
