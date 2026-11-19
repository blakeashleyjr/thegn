//! `thegn inbox list|test|status` (b163, THE-2214): the native inbox from the
//! command line. `list` reads `inbox_item` notification rows, `test` fetches
//! one account once (no DB writes, no events), and `status` prints the
//! daemon's `producers.status` reply.

use anyhow::Result;
use thegn_core::config::{Config, InboxAccount};
use thegn_core::inbox::{parse_source_ref, sanitize_item, sanitize_text};
use thegn_core::outln;
use thegn_core::producer_status::ProducersStatusReply;

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

const LIST_CAP: usize = 200;
const TEST_SHOW: usize = 10;
const NO_DAEMON: &str =
    "daemon not running: inbox polling needs the daemon with [daemon] headless_producers = true";

/// One stored inbox notification, as `list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ListRow {
    pub account: String,
    pub item_id: String,
    pub message: String,
    pub created_at: i64,
    pub read: bool,
}

/// Inbox rows from notification rows: `inbox_item` kind only, the account
/// from the source ref, newest first, capped.
pub(crate) fn list_rows(
    all: &[thegn_core::notification::Notification],
    account: Option<&str>,
    unread: bool,
) -> Vec<ListRow> {
    let mut rows: Vec<ListRow> = all
        .iter()
        .filter(|n| n.kind == thegn_core::notification::NotificationKind::InboxItem)
        .filter(|n| !unread || !n.read)
        .filter_map(|n| {
            let (acct, id) = parse_source_ref(&n.source_ref)?;
            if account.is_some_and(|a| a != acct) {
                return None;
            }
            Some(ListRow {
                account: acct.to_owned(),
                item_id: id.to_owned(),
                message: n.message.clone(),
                created_at: n.created_at_ms,
                read: n.read,
            })
        })
        .collect();
    rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    rows.truncate(LIST_CAP);
    rows
}

/// The message minus its `[account] ` prefix (the title).
fn title_of<'a>(row: &'a ListRow) -> &'a str {
    let prefix = format!("[{}] ", row.account);
    row.message.strip_prefix(&prefix).unwrap_or(&row.message)
}

pub(crate) fn render_list(rows: &[ListRow], json: bool, now: i64) -> Vec<String> {
    if json {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "account": r.account,
                    "item_id": r.item_id,
                    "message": r.message,
                    "created_at": r.created_at,
                    "read": r.read,
                })
            })
            .collect();
        return vec![serde_json::Value::Array(arr).to_string()];
    }
    rows.iter()
        .map(|r| {
            format!(
                "{}  [{}] {}",
                thegn_core::util::age_since(now, r.created_at),
                r.account,
                title_of(r)
            )
        })
        .collect()
}

/// The `test` guard inputs (the caller environment).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TestEnv {
    pub caller_set: bool,
    pub stage_launch_set: bool,
}

impl TestEnv {
    fn current() -> Self {
        TestEnv {
            caller_set: std::env::var_os("THEGN_CALLER").is_some(),
            stage_launch_set: std::env::var_os("THEGN_STAGE_LAUNCH").is_some(),
        }
    }
}

/// Refuse `inbox test` for a thegn caller, an unknown or a disabled account.
pub(crate) fn test_target<'a>(
    cfg: &'a Config,
    env: TestEnv,
    account: &str,
) -> Result<&'a InboxAccount> {
    if env.caller_set || env.stage_launch_set {
        anyhow::bail!("`thegn inbox test` is operator-only (refused for a thegn caller or stage)");
    }
    let acct = cfg
        .inbox
        .accounts
        .iter()
        .find(|a| a.name == account)
        .ok_or_else(|| anyhow::anyhow!("unknown inbox account `{account}`"))?;
    if !acct.enabled {
        anyhow::bail!("inbox account `{account}` is disabled");
    }
    Ok(acct)
}

fn scrub(msg: &str) -> String {
    sanitize_text(&thegn_core::log_redact::redact_text_line(msg), 200)
}

/// Render a fetch outcome: `(lines, ok)`.
pub(crate) fn render_test(
    kind: &str,
    result: &Result<thegn_core::inbox::InboxPage, thegn_svc::inbox::InboxError>,
    json: bool,
) -> (Vec<String>, bool) {
    match result {
        Ok(page) => {
            let items: Vec<_> = page
                .items
                .iter()
                .cloned()
                .filter_map(sanitize_item)
                .collect();
            if json {
                let shown: Vec<_> = items.iter().take(TEST_SHOW).collect();
                let v = serde_json::json!({
                    "kind": kind, "count": items.len(), "items": shown,
                });
                return (vec![v.to_string()], true);
            }
            let mut out = vec![format!("kind: {kind}"), format!("items: {}", items.len())];
            for item in items.iter().take(TEST_SHOW) {
                let url = if item.url.is_empty() {
                    String::new()
                } else {
                    format!("  {}", item.url)
                };
                out.push(format!("  {}  {}{url}", item.id, item.title));
            }
            (out, true)
        }
        Err(e) => {
            let class = format!("{:?}", e.class()).to_lowercase();
            let msg = scrub(&e.to_string());
            if json {
                let v = serde_json::json!({"kind": kind, "error_class": class, "error": msg});
                (vec![v.to_string()], false)
            } else {
                (
                    vec![format!("kind: {kind}"), format!("error ({class}): {msg}")],
                    false,
                )
            }
        }
    }
}

fn ms_age(ms: i64, now_ms: i64) -> String {
    if ms <= 0 {
        "never".to_owned()
    } else {
        format!(
            "{} ago",
            thegn_core::util::age_since(now_ms / 1000, ms / 1000)
        )
    }
}

/// Text lines for `status` from the daemon's reply (`None` = no daemon).
pub(crate) fn render_status(
    reply: Option<&ProducersStatusReply>,
    json: bool,
    now_ms: i64,
) -> Vec<String> {
    let Some(r) = reply else {
        return if json {
            vec![serde_json::json!({"daemon": false}).to_string()]
        } else {
            vec![NO_DAEMON.to_owned()]
        };
    };
    if json {
        return vec![serde_json::to_string(r).unwrap_or_default()];
    }
    let mut out = vec![format!(
        "headless_producers: {}  installed: {}",
        r.headless, r.installed
    )];
    for k in &r.kinds {
        out.push(format!(
            "  {:<10} armed={} lease={}",
            k.kind,
            k.armed,
            k.holder.as_deref().unwrap_or("none")
        ));
    }
    if r.inbox.is_empty() {
        out.push("inbox: no account polled yet".to_owned());
    }
    for a in &r.inbox {
        let err = if a.last_error.is_empty() {
            String::new()
        } else {
            format!("  error: {}", scrub(&a.last_error))
        };
        out.push(format!(
            "inbox [{}] {}: polled {}, ok {}, {} items{err}",
            a.account,
            a.kind,
            ms_age(a.last_poll_ms, now_ms),
            ms_age(a.last_ok_ms, now_ms),
            a.item_count
        ));
    }
    out
}

/// Ask a running daemon for `producers.status` (None when none answers).
/// Probes the unix socket only; never opens the state DB or spawns.
pub(crate) fn daemon_producers_status(cfg: &Config) -> Option<ProducersStatusReply> {
    use crate::daemon::client::{StopProbe, probe_stop_target};
    use thegn_svc::control::client::{ControlAddr, ControlClient};
    let socket = crate::daemon::socket_path(&cfg.daemon);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let client = ControlClient::new(ControlAddr::Unix(socket));
        let probe = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            probe_stop_target(&client, &crate::daemon::scope_aliases()),
        )
        .await;
        if !matches!(probe, Ok(StopProbe::Ours)) {
            return None;
        }
        let reply = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.call_raw("POST", "/v1/producers/status", Some(serde_json::json!({}))),
        )
        .await
        .ok()?
        .ok()?;
        serde_json::from_value(reply).ok()
    })
}

fn now_ms() -> i64 {
    thegn_core::util::now().saturating_mul(1000)
}

/// Run one `thegn inbox` verb.
pub fn run(cfg: &Config, action: Action) -> Result<()> {
    match action {
        Action::List {
            account,
            unread,
            json,
        } => {
            use thegn_core::store::NotificationStore;
            let all = match thegn_core::db::Db::open_read_only_current()? {
                Some(db) => db.get_all_notifications(5000)?,
                None => Vec::new(),
            };
            let rows = list_rows(&all, account.as_deref(), unread);
            for line in render_list(&rows, json, thegn_core::util::now()) {
                outln!("{line}");
            }
            Ok(())
        }
        Action::Test { account, json } => {
            let acct = test_target(cfg, TestEnv::current(), &account)?;
            let kind = acct.kind.as_str();
            let result = match thegn_svc::inbox::backend_for(acct) {
                Ok(backend) => {
                    let rt = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    rt.block_on(backend.fetch("", acct.max_items))
                }
                Err(e) => Err(e),
            };
            let (lines, ok) = render_test(kind, &result, json);
            for line in lines {
                outln!("{line}");
            }
            if ok {
                Ok(())
            } else {
                anyhow::bail!("inbox test failed for `{account}`")
            }
        }
        Action::Status { json } => {
            let reply = daemon_producers_status(cfg);
            for line in render_status(reply.as_ref(), json, now_ms()) {
                outln!("{line}");
            }
            Ok(())
        }
    }
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

    fn cfg_with(name: &str, enabled: bool) -> Config {
        let mut cfg = Config::default();
        cfg.inbox.accounts.push(InboxAccount {
            name: name.into(),
            enabled,
            ..Default::default()
        });
        cfg
    }

    #[test]
    fn test_refuses_callers_unknown_and_disabled() {
        let cfg = cfg_with("work", true);
        let ok = TestEnv::default();
        assert!(test_target(&cfg, ok, "work").is_ok());
        for env in [
            TestEnv {
                caller_set: true,
                stage_launch_set: false,
            },
            TestEnv {
                caller_set: false,
                stage_launch_set: true,
            },
        ] {
            let e = test_target(&cfg, env, "work").unwrap_err().to_string();
            assert!(e.contains("operator-only"), "{e}");
        }
        assert!(
            test_target(&cfg, ok, "nope")
                .unwrap_err()
                .to_string()
                .contains("unknown")
        );
        let off = cfg_with("work", false);
        assert!(
            test_target(&off, ok, "work")
                .unwrap_err()
                .to_string()
                .contains("disabled")
        );
    }

    fn note(db: &thegn_core::db::Db, kind: &str, account: &str, id: &str, msg: &str) -> i64 {
        use thegn_core::store::NotificationStore;
        db.put_notification(kind, &thegn_core::inbox::source_ref(account, id), msg, "")
            .unwrap()
    }

    #[test]
    fn list_filters_by_account_and_json_shape() {
        use thegn_core::store::NotificationStore;
        let _state = thegn_core::testenv::IsolatedState::new();
        let db = thegn_core::db::Db::open_memory().unwrap();
        note(&db, "inbox_item", "work", "a1", "[work] Review PR");
        note(&db, "inbox_item", "mail", "m1", "[mail] Hello");
        note(&db, "agent_done", "work", "x", "[work] not inbox");
        let all = db.get_all_notifications(100).unwrap();
        let rows = list_rows(&all, Some("work"), false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].item_id, "a1");
        assert_eq!(list_rows(&all, None, false).len(), 2);
        let text = render_list(&rows, false, rows[0].created_at + 120);
        assert_eq!(text, vec!["2m  [work] Review PR".to_owned()]);
        let json = render_list(&rows, true, 0);
        let v: serde_json::Value = serde_json::from_str(&json[0]).unwrap();
        let o = &v[0];
        assert_eq!(o["account"], "work");
        assert_eq!(o["item_id"], "a1");
        assert_eq!(o["message"], "[work] Review PR");
        assert_eq!(o["read"], false);
        assert!(o["created_at"].is_i64() && o.get("url").is_none());
    }

    #[test]
    fn status_text_from_a_canned_reply() {
        use thegn_core::producer_status::InboxAccountStatus;
        assert_eq!(render_status(None, false, 0), vec![NO_DAEMON.to_owned()]);
        let reply = ProducersStatusReply {
            installed: true,
            headless: true,
            kinds: vec![],
            inbox: vec![InboxAccountStatus {
                account: "work".into(),
                kind: "command".into(),
                last_poll_ms: 880_000,
                last_ok_ms: 0,
                last_error: "boom".into(),
                item_count: 3,
                baselined: false,
            }],
        };
        let lines = render_status(Some(&reply), false, 1_000_000);
        assert!(
            lines.iter().any(|l| l.contains("[work]")
                && l.contains("polled 2m ago")
                && l.contains("ok never")
                && l.contains("3 items")
                && l.contains("error: boom")),
            "{lines:?}"
        );
    }
}
