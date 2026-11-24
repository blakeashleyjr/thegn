//! The IMAP backend against a scripted loopback server (plain TCP through the
//! `cfg(test)` transport seam). Every test also asserts on the server's
//! captured command log that the client only ever read.

use super::*;
use std::fmt::Write as _;
use thegn_core::config::InboxKind;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;

const PASSWORD: &str = "hunter2-b163-sentinel";
const USER: &str = "me@x.org";

fn loopback(
    host: String,
    port: u16,
) -> BoxFuture<'static, Result<Box<dyn ImapStream>, InboxError>> {
    Box::pin(async move {
        let ip: std::net::IpAddr = host
            .parse()
            .map_err(|_| InboxError::Network("the test transport is loopback-only".into()))?;
        assert!(ip.is_loopback(), "the test transport is loopback-only");
        let tcp = tokio::net::TcpStream::connect((ip, port))
            .await
            .map_err(|e| InboxError::Network(e.to_string()))?;
        Ok(Box::new(tcp) as Box<dyn ImapStream>)
    })
}

/// What the fake server says.
#[derive(Clone)]
struct Script {
    caps: &'static str,
    login_ok: bool,
    search: &'static str,
    /// (uid, header block)
    messages: Vec<(u32, String)>,
    /// Replace every FETCH literal with an oversize announcement.
    hostile_literal: bool,
    /// Never answer anything (not even the greeting).
    silent: bool,
}

impl Default for Script {
    fn default() -> Self {
        Script {
            caps: "AUTH=PLAIN SASL-IR",
            login_ok: true,
            search: "3 5 9",
            messages: vec![
                (
                    3,
                    "From: old@x.org\r\nSubject: Old\r\n\r\n".to_owned(),
                ),
                (
                    5,
                    "From: bob@x.org\r\nSubject: Lunch\r\n\r\n".to_owned(),
                ),
                (
                    9,
                    "From: =?UTF-8?Q?Ren=C3=A9e?= <r@x.org>\r\nSubject: =?UTF-8?B?w6l0w6k=?=\r\n plans\r\nMessage-ID: <m9@x.org>\r\n\r\n"
                        .to_owned(),
                ),
            ],
            hostile_literal: false,
            silent: false,
        }
    }
}

/// Serve one connection; returns the command lines the client sent.
async fn serve(listener: TcpListener, script: Script) -> Vec<String> {
    let (stream, _) = listener.accept().await.expect("accept");
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut log = Vec::new();
    if script.silent {
        let mut line = String::new();
        // Wait for the client to give up.
        while read
            .read_line(&mut line)
            .await
            .map(|n| n > 0)
            .unwrap_or(false)
        {
            line.clear();
        }
        return log;
    }
    write
        .write_all(b"* OK fake IMAP ready\r\n")
        .await
        .expect("greeting");
    loop {
        let mut line = String::new();
        match read.read_line(&mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = line.trim_end_matches("\r\n").to_owned();
        log.push(line.clone());
        let mut words = line.split(' ');
        let tag = words.next().unwrap_or_default().to_owned();
        let verb = words.next().unwrap_or_default().to_ascii_uppercase();
        let sub = words.next().unwrap_or_default().to_ascii_uppercase();
        let reply = match (verb.as_str(), sub.as_str()) {
            ("CAPABILITY", _) => format!(
                "* CAPABILITY IMAP4rev1 {}\r\n{tag} OK done\r\n",
                script.caps
            ),
            ("LOGIN", _) | ("AUTHENTICATE", _) => {
                if script.login_ok {
                    format!("{tag} OK logged in\r\n")
                } else {
                    // A hostile server echoes the credential back.
                    format!(
                        "{tag} NO [AUTHENTICATIONFAILED] bad password {PASSWORD} for {USER}\r\n"
                    )
                }
            }
            ("EXAMINE", _) => format!(
                "* 3 EXISTS\r\n* FLAGS (\\Seen)\r\n* OK [UIDVALIDITY 77] UIDs valid\r\n{tag} OK [READ-ONLY] done\r\n"
            ),
            ("UID", "SEARCH") => format!("* SEARCH {}\r\n{tag} OK done\r\n", script.search),
            ("UID", "FETCH") => {
                let set: Vec<u32> = words
                    .next()
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(|n| n.parse().ok())
                    .collect();
                let mut out = String::from("* 1 FETCH (FLAGS (\\Seen))\r\n");
                for (seq, (uid, header)) in script.messages.iter().enumerate() {
                    if !set.contains(uid) {
                        continue;
                    }
                    let literal = if script.hostile_literal {
                        format!("{{{}}}\r\n", proto::MAX_LITERAL + 1)
                    } else {
                        format!("{{{}}}\r\n{header}", header.len())
                    };
                    writeln!(
                        out,
                        "* {} FETCH (UID {uid} FLAGS () INTERNALDATE \"17-Jul-1996 02:44:25 +0000\" BODY[HEADER.FIELDS (FROM SUBJECT MESSAGE-ID)] {literal})\r",
                        seq + 1
                    )
                    .expect("writing to String cannot fail");
                }
                writeln!(out, "{tag} OK done\r").expect("writing to String cannot fail");
                out
            }
            ("LOGOUT", _) => {
                let reply = format!("* BYE bye\r\n{tag} OK done\r\n");
                if write.write_all(reply.as_bytes()).await.is_err() {
                    // The client may already be gone.
                }
                break;
            }
            _ => format!("{tag} BAD unknown command\r\n"),
        };
        if write.write_all(reply.as_bytes()).await.is_err() {
            break;
        }
    }
    log
}

struct Fixture {
    _dir: tempfile::TempDir,
    backend: ImapBackend,
    server: tokio::task::JoinHandle<Vec<String>>,
}

async fn fixture(script: Script, unseen_only: bool) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("imap-password");
    std::fs::write(&path, PASSWORD).expect("write password");
    thegn_core::fsperm::restrict_to_owner(&path).expect("restrict password file");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let account = InboxAccount {
        name: "mail".into(),
        kind: InboxKind::Imap,
        host: "127.0.0.1".into(),
        port,
        username: USER.into(),
        password: format!("file:{}", path.display()),
        unseen_only,
        timeout_secs: 10,
        ..Default::default()
    };
    let mut backend = ImapBackend::from_account(&account).expect("configured");
    backend.connect = Some(loopback as Connector);
    let server = tokio::spawn(serve(listener, script));
    Fixture {
        _dir: dir,
        backend,
        server,
    }
}

/// The client never sends a command that could change mail or mark it seen.
fn assert_read_only(log: &[String]) {
    for line in log {
        let upper = line.to_ascii_uppercase();
        let verb = upper.split(' ').nth(1).unwrap_or_default();
        assert!(
            !matches!(
                verb,
                "SELECT" | "STORE" | "EXPUNGE" | "COPY" | "MOVE" | "APPEND" | "DELETE"
            ),
            "mutating command sent: {line}"
        );
        for word in [" STORE ", "+FLAGS", "-FLAGS", "BODY["] {
            assert!(!upper.contains(word), "{word} sent: {line}");
        }
        if upper.contains("FETCH") {
            assert!(upper.contains("BODY.PEEK["), "FETCH without PEEK: {line}");
        }
    }
}

fn verbs(log: &[String]) -> Vec<String> {
    log.iter()
        .map(|line| {
            let mut words = line.split(' ').skip(1);
            let verb = words.next().unwrap_or_default();
            if verb == "UID" {
                format!("UID {}", words.next().unwrap_or_default())
            } else {
                verb.to_owned()
            }
        })
        .collect()
}

#[tokio::test]
async fn authenticate_plain_examine_search_fetch_yields_newest_items() {
    let f = fixture(Script::default(), true).await;
    let page = f.backend.fetch("", 2).await.expect("fetch");
    let log = f.server.await.expect("server");
    assert_read_only(&log);
    assert_eq!(
        verbs(&log),
        [
            "CAPABILITY",
            "AUTHENTICATE",
            "EXAMINE",
            "UID SEARCH",
            "UID FETCH",
            "LOGOUT"
        ]
    );
    assert!(log[1].starts_with("A2 AUTHENTICATE PLAIN "), "{}", log[1]);
    assert_eq!(log[2], "A3 EXAMINE \"INBOX\"");
    assert_eq!(log[3], "A4 UID SEARCH UNSEEN");
    assert!(
        log[4].starts_with("A5 UID FETCH 5,9 "),
        "newest two: {}",
        log[4]
    );

    assert_eq!(page.cursor.as_deref(), Some("77"));
    assert_eq!(page.mode, PageMode::Snapshot);
    assert!(page.truncated, "three matched, two taken");
    let ids: Vec<_> = page.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["77-9", "77-5"], "newest first");
    let newest = &page.items[0];
    assert_eq!(newest.title, "été plans");
    assert_eq!(newest.from, "Renée");
    assert_eq!(newest.url, "", "not a Gmail host");
    assert_eq!(newest.updated_at, Some(837_571_465));
    assert_eq!(page.items[1].title, "Lunch");
    assert_eq!(page.items[1].from, "bob@x.org");
    // Everything survives the daemon's sanitizer.
    assert!(
        page.items
            .iter()
            .all(|i| thegn_core::inbox::sanitize_item(i.clone()).is_some())
    );
}

#[tokio::test]
async fn login_fallback_quotes_credentials_and_since_search() {
    let script = Script {
        caps: "IDLE",
        ..Script::default()
    };
    let f = fixture(script, false).await;
    let page = f.backend.fetch("", 50).await.expect("fetch");
    let log = f.server.await.expect("server");
    assert_read_only(&log);
    assert_eq!(
        verbs(&log),
        [
            "CAPABILITY",
            "LOGIN",
            "EXAMINE",
            "UID SEARCH",
            "UID FETCH",
            "LOGOUT"
        ]
    );
    assert_eq!(log[1], format!("A2 LOGIN \"{USER}\" \"{PASSWORD}\""));
    assert!(log[3].starts_with("A4 UID SEARCH SINCE "), "{}", log[3]);
    assert!(!page.truncated);
    assert_eq!(page.items.len(), 3);
}

#[tokio::test]
async fn auth_failure_is_auth_and_never_carries_the_password() {
    let script = Script {
        login_ok: false,
        ..Script::default()
    };
    let f = fixture(script, true).await;
    let err = f.backend.fetch("", 10).await.expect_err("rejected");
    let log = f.server.await.expect("server");
    assert_read_only(&log);
    match &err {
        InboxError::Auth(message) => {
            assert!(message.contains("AUTHENTICATIONFAILED"), "{message}")
        }
        other => panic!("expected Auth, got {other:?}"),
    }
    for rendered in [err.to_string(), format!("{err:?}")] {
        assert!(!rendered.contains(PASSWORD), "password leaked: {rendered}");
        assert!(!rendered.contains(USER), "login name leaked: {rendered}");
    }
    assert_eq!(err.class(), super::super::InboxErrorClass::Auth);
}

#[tokio::test]
async fn logindisabled_sends_no_credentials() {
    let script = Script {
        caps: "LOGINDISABLED AUTH=PLAIN SASL-IR",
        ..Script::default()
    };
    let f = fixture(script, true).await;
    let err = f.backend.fetch("", 10).await.expect_err("refused");
    let log = f.server.await.expect("server");
    assert!(matches!(err, InboxError::Auth(_)), "{err:?}");
    assert_eq!(verbs(&log), ["CAPABILITY"]);
    assert!(log.iter().all(|line| !line.contains(PASSWORD)));
}

#[tokio::test]
async fn hostile_oversize_literal_is_a_parse_error() {
    let script = Script {
        hostile_literal: true,
        ..Script::default()
    };
    let f = fixture(script, true).await;
    let err = f.backend.fetch("", 10).await.expect_err("bounded");
    match &err {
        InboxError::Parse(message) => assert!(message.contains("literal"), "{message}"),
        other => panic!("expected Parse, got {other:?}"),
    }
    let log = f.server.await.expect("server");
    assert_read_only(&log);
}

#[tokio::test]
async fn a_silent_server_times_out() {
    let script = Script {
        silent: true,
        ..Script::default()
    };
    let mut f = fixture(script, true).await;
    f.backend.timeout = Duration::from_millis(300);
    let err = f.backend.fetch("", 10).await.expect_err("deadline");
    assert_eq!(err, InboxError::Timeout);
    // The client dropped the connection, so the server's read ends.
    assert!(f.server.await.expect("server").is_empty());
}

#[tokio::test]
async fn unresolved_or_literal_password_is_auth_before_connecting() {
    let mut account = InboxAccount {
        name: "mail".into(),
        kind: InboxKind::Imap,
        host: "127.0.0.1".into(),
        port: 1,
        username: USER.into(),
        password: "keyring:thegn-b163-imap-test-no-such-entry".into(),
        ..Default::default()
    };
    let mut backend = ImapBackend::from_account(&account).expect("configured");
    backend.connect = Some(loopback as Connector);
    let err = backend.fetch("", 10).await.expect_err("unresolved");
    assert!(matches!(err, InboxError::Auth(_)), "{err:?}");

    account.password = PASSWORD.into();
    let mut backend = ImapBackend::from_account(&account).expect("configured");
    backend.connect = Some(loopback as Connector);
    let err = backend.fetch("", 10).await.expect_err("literal refused");
    assert!(matches!(err, InboxError::Auth(_)), "{err:?}");
    assert!(!err.to_string().contains(PASSWORD));
}

#[test]
fn gmail_items_link_by_message_id() {
    let account = InboxAccount {
        name: "mail".into(),
        kind: InboxKind::Imap,
        host: "IMAP.gmail.com".into(),
        username: USER.into(),
        password: "env:IMAP_PASS".into(),
        ..Default::default()
    };
    let backend = ImapBackend::from_account(&account).expect("configured");
    let header = b"Subject: Hi\r\nMessage-ID: <a b@x.org>\r\n\r\n";
    let item = backend.item(4, 12, header, None);
    assert_eq!(item.id, "4-12");
    assert_eq!(
        item.url,
        "https://mail.google.com/mail/u/0/#search/rfc822msgid:a%20b@x.org"
    );
    let item = backend.item(4, 13, b"Subject:\r\n\r\n", None);
    assert_eq!(
        (item.url.as_str(), item.title.as_str()),
        ("", "(no subject)")
    );
}

#[test]
fn from_account_and_probe_need_host_username_and_a_password_ref() {
    let mut account = InboxAccount {
        name: "mail".into(),
        kind: InboxKind::Imap,
        ..Default::default()
    };
    assert_eq!(
        ImapBackend::from_account(&account)
            .err()
            .map(|e| e.to_string()),
        Some(InboxError::NotConfigured.to_string())
    );
    assert!(
        probe(&account, "imap:mail".into())
            .availability
            .is_unavailable()
    );

    account.host = "imap.example.com".into();
    account.username = USER.into();
    account.password = PASSWORD.into();
    assert!(
        probe(&account, "imap:mail".into())
            .availability
            .is_unavailable(),
        "a literal password is not a ref"
    );

    account.password = "keyring:imap-mail".into();
    let report = probe(&account, "imap:mail".into());
    assert!(report.availability.is_ready());
    assert!(
        report
            .notes
            .iter()
            .any(|n| n == "read-only (EXAMINE, BODY.PEEK)")
    );
    let backend = ImapBackend::from_account(&account).expect("configured");
    assert_eq!(backend.port, DEFAULT_PORT);
    assert_eq!(backend.mailbox, "INBOX");
    assert_eq!(backend.timeout, Duration::from_secs(30));
    assert_eq!(backend.kind(), "imap");
    assert!(!backend.caps().mark_read);
}
