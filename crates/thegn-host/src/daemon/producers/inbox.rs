//! The daemon's inbox producer (b163, THE-2214): polls each enabled
//! `[[inbox.accounts]]` entry of an implemented kind, diffs against the
//! account's seen-set (`inbox_account_state`), writes `inbox_item`
//! notification rows and submits `inbox_item` facts for new ids after the
//! baseline. Blocking; the owner runs it in `spawn_blocking`.
//!
//! - Accounts are polled when due (`last_poll_ms` + the account's effective
//!   interval), at most [`MAX_ACCOUNTS_PER_TICK`] per tick, rotating by name
//!   so none starves, and no account is started once its fetch could run past
//!   [`TICK_BUDGET`] (the lease is renewed after the tick).
//! - The first successful poll of an account, or the first after its
//!   `config_digest` changed, is a baseline: at most
//!   `inbox::FIRST_POLL_SURFACE` rows are written directly, with no routing,
//!   no push and no automation event of any kind.
//! - Later polls emit each new item through the routed once-emitter (keyed on
//!   kind, source ref and message, so a crash between the emit and the state
//!   write cannot double-insert) and submit one `inbox_item` fact for it.
//! - Every item passes `inbox::sanitize_item` first. Item text reaches the
//!   notification `message` and `InboxFacts` (whose title rules see only as
//!   `message`); it never becomes argv, env, a path or a rule.
//! - Paused automation stops inbox polling too: the owner skips every
//!   producer's tick while paused.
//! - Offline (connectivity gate), the tick does nothing.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use thegn_core::automation::AutomationEventKind;
use thegn_core::config::InboxAccount;
use thegn_core::db::Db;
use thegn_core::db_inbox::InboxAccountState;
use thegn_core::inbox::{InboxPage, diff, facts, message, sanitize_item, source_ref};
use thegn_core::seam::Kind as _;
use thegn_core::store::NotificationStore;
use thegn_svc::inbox::{InboxBackend, InboxError};

use super::{TickCtx, TickOutcome};
use crate::automation_events::EventFacts;

/// At most this many accounts are polled per tick.
pub(crate) const MAX_ACCOUNTS_PER_TICK: usize = 16;

/// No account is started once its fetch timeout would carry the tick past
/// this. Below the shortest inbox lease (three 60 s cadences).
const TICK_BUDGET: Duration = Duration::from_secs(120);

/// The longest persisted cursor; a longer one is dropped (refetch from
/// scratch) rather than stored.
const MAX_CURSOR_CHARS: usize = 4096;

/// The notification kind (and fact kind) of inbox items.
const KIND: &str = "inbox_item";

/// The last account name polled; the next tick starts strictly after it.
static CURSOR: Mutex<Option<String>> = Mutex::new(None);

/// Builds the backend for an account (`thegn_svc::inbox::backend_for`;
/// tests inject a fake).
pub(crate) type BackendFor<'a> =
    &'a dyn Fn(&InboxAccount) -> Result<Box<dyn InboxBackend>, InboxError>;

/// Submits one automation fact (`automation_events::submit_fact`; tests
/// count calls).
pub(crate) type SubmitFact<'a> =
    &'a dyn Fn(AutomationEventKind, String, Option<String>, Option<String>, EventFacts);

/// One inbox tick. Blocking.
pub(crate) fn tick(ctx: &TickCtx) -> TickOutcome {
    if crate::connectivity_gate::should_skip_refresh(
        &crate::hydrate::RefreshKind::Issues,
        thegn_core::connectivity::current(),
    ) {
        return TickOutcome::Skipped("offline");
    }
    tick_with(
        ctx,
        &thegn_svc::inbox::backend_for,
        &|kind, key, wt, msg, f| {
            crate::automation_events::submit_fact(kind, key, wt, msg, f);
        },
    )
}

/// The tick with its two side doors injected.
pub(crate) fn tick_with(
    ctx: &TickCtx,
    backend_for: BackendFor<'_>,
    submit: SubmitFact<'_>,
) -> TickOutcome {
    let inbox = &ctx.cfg.inbox;
    if !inbox.enabled {
        return TickOutcome::Skipped("inbox disabled");
    }
    let Ok(db) = Db::open() else {
        return TickOutcome::Skipped("database unavailable");
    };
    let accounts: Vec<&InboxAccount> = inbox.implemented_enabled().collect();
    let keep: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
    if let Err(error) = db.prune_inbox_states(&keep) {
        tracing::warn!(target: "thegn::producers", %error, "inbox state prune failed");
    }
    let mut due: Vec<(&InboxAccount, Option<InboxAccountState>)> = Vec::new();
    for account in accounts {
        let state = match db.inbox_state(&account.name) {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(target: "thegn::producers", account = %account.name, %error, "inbox state unreadable");
                continue;
            }
        };
        let interval_ms = ms(account.effective_poll_secs(inbox.poll_interval_secs));
        if is_due(state.as_ref(), interval_ms, ctx.now_ms) {
            due.push((account, state));
        }
    }
    due.sort_by(|a, b| a.0.name.cmp(&b.0.name));
    let names: Vec<&str> = due.iter().map(|(a, _)| a.name.as_str()).collect();
    let start = {
        let cursor = CURSOR
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        rotation_start(&names, cursor.as_deref())
    };
    let started = Instant::now();
    let mut work = 0;
    for (account, state) in due
        .iter()
        .cycle()
        .skip(start)
        .take(due.len().min(MAX_ACCOUNTS_PER_TICK))
    {
        if started.elapsed() + fetch_timeout(account) > TICK_BUDGET {
            break;
        }
        work += poll_account(&db, account, state.clone(), ctx.now_ms, backend_for, submit);
        *CURSOR
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(account.name.clone());
    }
    TickOutcome::Done { work }
}

fn ms(secs: u64) -> i64 {
    i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX)
}

/// Due when never polled, when the interval has passed, or when the stored
/// poll time is further ahead than one interval (the clock stepped back).
fn is_due(state: Option<&InboxAccountState>, interval_ms: i64, now_ms: i64) -> bool {
    let Some(state) = state else { return true };
    let next = state.last_poll_ms.saturating_add(interval_ms);
    next <= now_ms || state.last_poll_ms > now_ms.saturating_add(interval_ms)
}

/// Index of the first name strictly after `after` (wrapping to 0).
fn rotation_start(sorted: &[&str], after: Option<&str>) -> usize {
    after.map_or(0, |after| {
        sorted.iter().position(|name| *name > after).unwrap_or(0)
    })
}

fn fetch_timeout(account: &InboxAccount) -> Duration {
    Duration::from_secs(
        account
            .timeout_secs
            .clamp(1, thegn_core::config_inbox::MAX_TIMEOUT_SECS),
    )
}

/// Run one fetch on the shared background runtime (no runtime of our own,
/// no thread of our own), bounded by the account's timeout.
fn fetch(
    backend: &dyn InboxBackend,
    cursor: &str,
    max_items: usize,
    timeout: Duration,
) -> Result<InboxPage, InboxError> {
    thegn_svc::bg_runtime::block_on(async {
        tokio::time::timeout(timeout, backend.fetch(cursor, max_items)).await
    })
    .unwrap_or_else(|| Ok(Err(InboxError::Io("background runtime unavailable".into()))))
    .unwrap_or(Err(InboxError::Timeout))
}

/// Poll one account and persist its state. Returns the number of new items
/// surfaced after the baseline (baseline rows are not counted as work).
fn poll_account(
    db: &Db,
    account: &InboxAccount,
    state: Option<InboxAccountState>,
    now_ms: i64,
    backend_for: BackendFor<'_>,
    submit: SubmitFact<'_>,
) -> usize {
    let digest = account.config_digest();
    let mut state = state.unwrap_or_else(|| InboxAccountState {
        account: account.name.clone(),
        ..Default::default()
    });
    state.kind = account.kind.as_str().to_owned();
    let digest_changed = state.baselined && state.config_digest != digest;
    // A different source starts from scratch.
    let cursor = if digest_changed {
        String::new()
    } else {
        state.cursor.clone()
    };
    let max_items = account
        .max_items
        .clamp(1, thegn_core::config_inbox::MAX_ITEMS_CAP);
    let result = backend_for(account)
        .and_then(|backend| fetch(backend.as_ref(), &cursor, max_items, fetch_timeout(account)));
    let page = match result {
        Ok(page) => page,
        Err(error) => {
            record_error(db, state, now_ms, &error.to_string(), error.class());
            return 0;
        }
    };
    let page = sanitize_page(page);
    let d = diff(&state.seen, state.baselined, digest_changed, &page);
    let kind = account.kind.as_str();
    let mut work = 0;
    for item in &d.new {
        let sref = source_ref(&account.name, &item.id);
        let msg = message(&account.name, item);
        if d.baseline {
            // Rows only: no routing, no push, no automation event.
            if let Err(error) = db.put_notification_once(KIND, &sref, &msg, "") {
                record_write_error(db, state, now_ms, &error);
                return work;
            }
            continue;
        }
        let inserted = match crate::automation_events::emit_once(db, KIND, &sref, &msg, "") {
            Ok(inserted) => inserted,
            Err(error) => {
                record_write_error(db, state, now_ms, &error);
                return work;
            }
        };
        // Not inserted: either a row for this exact item and message exists
        // (a replay after a crash before the state write: skip), or routing
        // declined to record it (no row at all: rules still see the fact).
        let fire = inserted || !db.has_notification(KIND, &sref).unwrap_or(true);
        if fire {
            submit(
                AutomationEventKind::InboxItem,
                sref,
                None,
                Some(msg),
                EventFacts {
                    inbox: Some(facts(kind, &account.name, item)),
                    ..Default::default()
                },
            );
            work += 1;
        }
    }
    let item_count = i64::try_from(page.items.len()).unwrap_or(i64::MAX);
    let next = InboxAccountState {
        config_digest: digest,
        cursor: page.cursor.unwrap_or_default(),
        seen: d.seen,
        baselined: true,
        last_poll_ms: now_ms,
        last_ok_ms: now_ms,
        last_error: String::new(),
        item_count,
        ..state
    };
    if let Err(error) = db.put_inbox_state(&next) {
        tracing::warn!(target: "thegn::producers", account = %account.name, %error, "inbox state write failed");
    }
    work
}

/// Sanitize every item (dropping the invalid), keep only well-formed
/// removed ids, and drop an over-long or control-laden cursor.
fn sanitize_page(page: InboxPage) -> InboxPage {
    let valid_id = |id: &String| {
        sanitize_item(thegn_core::inbox::InboxItem {
            id: id.clone(),
            ..Default::default()
        })
        .is_some_and(|clean| clean.id == *id)
    };
    InboxPage {
        items: page.items.into_iter().filter_map(sanitize_item).collect(),
        removed: page.removed.into_iter().filter(valid_id).collect(),
        cursor: page
            .cursor
            .filter(|c| c.chars().count() <= MAX_CURSOR_CHARS && !c.chars().any(char::is_control)),
        mode: page.mode,
        truncated: page.truncated,
    }
}

/// A failed fetch: stamp the poll time and the scrubbed error, nothing else
/// (the seen-set, cursor and baseline survive).
fn record_error(
    db: &Db,
    state: InboxAccountState,
    now_ms: i64,
    error: &str,
    class: thegn_svc::inbox::InboxErrorClass,
) {
    tracing::debug!(target: "thegn::producers", account = %state.account, ?class, "inbox fetch failed");
    let next = InboxAccountState {
        last_poll_ms: now_ms,
        // `put_inbox_state` strips control characters and caps the length.
        last_error: thegn_core::ci_log::redact(error),
        ..state
    };
    if let Err(error) = db.put_inbox_state(&next) {
        tracing::warn!(target: "thegn::producers", account = %next.account, %error, "inbox state write failed");
    }
}

fn record_write_error(db: &Db, state: InboxAccountState, now_ms: i64, error: &anyhow::Error) {
    tracing::warn!(target: "thegn::producers", account = %state.account, %error, "inbox notification write failed");
    record_error(
        db,
        state,
        now_ms,
        "notification write failed",
        thegn_svc::inbox::InboxErrorClass::Transient,
    );
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::Arc;

    use futures::future::BoxFuture;
    use thegn_core::config::{Config, InboxKind};
    use thegn_core::inbox::{InboxItem, PageMode};
    use thegn_svc::inbox::InboxCaps;

    use super::*;

    /// Serialises the tests that share [`CURSOR`].
    static SERIAL: Mutex<()> = Mutex::new(());

    struct Fake(Result<InboxPage, InboxError>);

    impl InboxBackend for Fake {
        fn kind(&self) -> &'static str {
            "command"
        }
        fn caps(&self) -> InboxCaps {
            InboxCaps::default()
        }
        fn fetch<'a>(
            &'a self,
            _cursor: &'a str,
            _max_items: usize,
        ) -> BoxFuture<'a, Result<InboxPage, InboxError>> {
            let result = self.0.clone();
            Box::pin(async move { result })
        }
    }

    fn cfg(accounts: &[&str]) -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.inbox.enabled = true;
        cfg.inbox.accounts = accounts
            .iter()
            .map(|name| InboxAccount {
                name: (*name).into(),
                kind: InboxKind::Command,
                command: vec!["true".into()],
                ..Default::default()
            })
            .collect();
        Arc::new(cfg)
    }

    fn page(ids: impl IntoIterator<Item = usize>) -> InboxPage {
        InboxPage {
            items: ids
                .into_iter()
                .map(|i| InboxItem {
                    id: format!("m{i}"),
                    title: format!("item {i}\u{202E}"),
                    updated_at: Some(i as i64),
                    ..Default::default()
                })
                .collect(),
            mode: PageMode::Snapshot,
            ..Default::default()
        }
    }

    fn rows(db: &Db) -> Vec<thegn_core::notification::Notification> {
        db.get_all_notifications(10_000)
            .unwrap()
            .into_iter()
            .filter(|n| n.kind.as_str() == KIND)
            .collect()
    }

    /// Run one tick against a fixed backend result; returns (outcome, facts).
    fn run(
        cfg: &Arc<Config>,
        now_ms: i64,
        result: Result<InboxPage, InboxError>,
    ) -> (TickOutcome, Vec<(String, EventFacts, Option<String>)>) {
        let submitted = RefCell::new(Vec::new());
        let outcome = tick_with(
            &TickCtx {
                cfg: cfg.clone(),
                now_ms,
            },
            &|_| Ok(Box::new(Fake(result.clone())) as Box<dyn InboxBackend>),
            &|kind, key, _wt, msg, facts| {
                assert_eq!(kind, AutomationEventKind::InboxItem);
                submitted.borrow_mut().push((key, facts, msg));
            },
        );
        (outcome, submitted.into_inner())
    }

    #[test]
    fn first_poll_baselines_then_one_new_item_is_one_row_and_one_fact() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _state = crate::testenv::IsolatedState::new();
        let cfg = cfg(&["work"]);
        let (outcome, submitted) = run(&cfg, 1_000_000, Ok(page(0..30)));
        assert_eq!(outcome, TickOutcome::Done { work: 0 });
        assert!(submitted.is_empty(), "no fact on the baseline");
        let db = Db::open().unwrap();
        let first = rows(&db);
        assert_eq!(first.len(), thegn_core::inbox::FIRST_POLL_SURFACE);
        assert!(first.iter().all(|n| !n.message.contains('\u{202E}')));
        let state = db.inbox_state("work").unwrap().unwrap();
        assert!(state.baselined);
        assert_eq!(state.seen.len(), 30);
        assert_eq!(state.item_count, 30);

        // Not due yet: nothing happens.
        let (outcome, submitted) = run(&cfg, 1_000_001, Ok(page(0..31)));
        assert_eq!(outcome, TickOutcome::Done { work: 0 });
        assert!(submitted.is_empty());

        // Due: one new id is one row and one fact.
        let later = 1_000_000 + 300_000;
        let (outcome, submitted) = run(&cfg, later, Ok(page(0..31)));
        assert_eq!(outcome, TickOutcome::Done { work: 1 });
        assert_eq!(rows(&db).len(), first.len() + 1);
        assert_eq!(submitted.len(), 1);
        let (key, facts, msg) = &submitted[0];
        assert_eq!(key, "inbox:work:m30");
        assert_eq!(msg.as_deref(), Some("[work] item 30"));
        let inbox = facts.inbox.as_ref().unwrap();
        assert_eq!(
            (
                inbox.account.as_str(),
                inbox.item_id.as_str(),
                inbox.kind.as_str()
            ),
            ("work", "m30", "command")
        );
    }

    #[test]
    fn a_replayed_new_item_is_not_inserted_or_fired_twice() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _state = crate::testenv::IsolatedState::new();
        let cfg = cfg(&["work"]);
        run(&cfg, 0, Ok(page(0..2)));
        let db = Db::open().unwrap();
        // A crash after the emit but before the state write: the row exists,
        // the seen-set does not have the id.
        let item = &page(2..3).items[0];
        let sanitized = sanitize_item(item.clone()).unwrap();
        db.put_notification_once(KIND, "inbox:work:m2", &message("work", &sanitized), "")
            .unwrap();
        let (_, submitted) = run(&cfg, 300_000, Ok(page(0..3)));
        assert!(submitted.is_empty(), "the replay fires nothing");
        assert_eq!(rows(&db).len(), 3);
        assert!(
            db.inbox_state("work")
                .unwrap()
                .unwrap()
                .seen
                .contains(&"m2".to_owned())
        );
    }

    #[test]
    fn an_error_records_last_error_and_keeps_seen() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _state = crate::testenv::IsolatedState::new();
        let cfg = cfg(&["work"]);
        run(&cfg, 0, Ok(page(0..3)));
        let db = Db::open().unwrap();
        let before = db.inbox_state("work").unwrap().unwrap();
        let (outcome, submitted) = run(
            &cfg,
            300_000,
            Err(InboxError::Auth(format!(
                "bad\u{1b}[31m token {}",
                "x".repeat(400)
            ))),
        );
        assert_eq!(outcome, TickOutcome::Done { work: 0 });
        assert!(submitted.is_empty());
        let after = db.inbox_state("work").unwrap().unwrap();
        assert_eq!(after.seen, before.seen);
        assert_eq!(after.cursor, before.cursor);
        assert_eq!(after.last_ok_ms, before.last_ok_ms);
        assert!(after.baselined);
        assert_eq!(after.last_poll_ms, 300_000);
        assert!(after.last_error.starts_with("authentication failed"));
        assert!(after.last_error.chars().count() <= 200);
        assert!(!after.last_error.chars().any(char::is_control));
    }

    #[test]
    fn a_disabled_accounts_state_is_pruned() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _state = crate::testenv::IsolatedState::new();
        run(&cfg(&["a", "b"]), 0, Ok(page(0..1)));
        let db = Db::open().unwrap();
        assert_eq!(db.inbox_states().unwrap().len(), 2);
        let mut only_a = (*cfg(&["a", "b"])).clone();
        only_a.inbox.accounts[1].enabled = false;
        run(&Arc::new(only_a), 1, Ok(page(0..1)));
        let names: Vec<_> = db
            .inbox_states()
            .unwrap()
            .into_iter()
            .map(|s| s.account)
            .collect();
        assert_eq!(names, ["a"]);
    }

    #[test]
    fn a_digest_change_rebaselines_without_facts() {
        let _serial = SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _state = crate::testenv::IsolatedState::new();
        let cfg1 = cfg(&["work"]);
        run(&cfg1, 0, Ok(page(0..2)));
        let mut changed = (*cfg1).clone();
        changed.inbox.accounts[0].command = vec!["other".into()];
        let (_, submitted) = run(&Arc::new(changed), 300_000, Ok(page(5..8)));
        assert!(submitted.is_empty(), "a new source is a baseline");
    }

    #[test]
    fn disabled_inbox_ticks_nothing() {
        let mut off = (*cfg(&["work"])).clone();
        off.inbox.enabled = false;
        let (outcome, submitted) = run(&Arc::new(off), 0, Ok(page(0..1)));
        assert_eq!(outcome, TickOutcome::Skipped("inbox disabled"));
        assert!(submitted.is_empty());
    }

    #[test]
    fn due_and_rotation_rules() {
        let state = |last_poll_ms| InboxAccountState {
            last_poll_ms,
            ..Default::default()
        };
        assert!(is_due(None, 60_000, 0));
        assert!(!is_due(Some(&state(1_000)), 60_000, 60_999), "not due");
        assert!(is_due(Some(&state(1_000)), 60_000, 61_000));
        assert!(
            is_due(Some(&state(10_000_000)), 60_000, 1_000),
            "a poll time from a clock that stepped back"
        );
        let names = ["a", "b", "c"];
        assert_eq!(rotation_start(&names, None), 0);
        assert_eq!(rotation_start(&names, Some("a")), 1);
        assert_eq!(rotation_start(&names, Some("c")), 0, "wraps");
        assert_eq!(rotation_start(&names, Some("bb")), 2, "a removed name");
    }

    #[test]
    fn sanitize_page_drops_bad_ids_and_cursors() {
        let raw = InboxPage {
            items: vec![
                InboxItem {
                    id: "ok".into(),
                    ..Default::default()
                },
                InboxItem {
                    id: "bad id".into(),
                    ..Default::default()
                },
            ],
            removed: vec!["x".into(), "../etc".into(), "y\n".into()],
            cursor: Some("c\u{7}".into()),
            mode: PageMode::Delta,
            truncated: true,
        };
        let clean = sanitize_page(raw);
        assert_eq!(clean.items.len(), 1);
        assert_eq!(clean.removed, ["x"]);
        assert_eq!(clean.cursor, None);
        assert!(clean.truncated && clean.mode == PageMode::Delta);
        let long = sanitize_page(InboxPage {
            cursor: Some("c".repeat(MAX_CURSOR_CHARS + 1)),
            ..Default::default()
        });
        assert_eq!(long.cursor, None);
    }
}
