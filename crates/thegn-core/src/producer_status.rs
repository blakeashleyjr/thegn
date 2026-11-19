//! The `producers.status` reply (b163, THE-2214): which headless producer
//! kinds the daemon has armed, who holds each lease, and every inbox
//! account's last poll. Read by `thegn inbox status`, the Lead (MCP
//! `producers_status`) and doctor. Error strings are scrubbed and capped
//! before they are stored (`db_inbox`), so nothing here carries a credential
//! or raw third-party text.

use serde::{Deserialize, Serialize};

/// The whole reply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProducersStatusReply {
    /// The daemon's producer owner is installed (`headless_producers` on).
    pub installed: bool,
    /// `[daemon] headless_producers`.
    pub headless: bool,
    /// One row per producer kind, in `ProducerKind::ALL` order.
    pub kinds: Vec<ProducerKindStatus>,
    /// One row per inbox account with stored state, ordered by account.
    pub inbox: Vec<InboxAccountStatus>,
}

/// One producer kind.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ProducerKindStatus {
    /// `ProducerKind::as_str`.
    pub kind: String,
    /// The daemon runs this kind's task.
    pub armed: bool,
    /// The lease holder, when a lease row exists.
    pub holder: Option<String>,
    pub holder_pid: Option<i64>,
    pub expires_at_ms: Option<i64>,
    /// The lease exists and has not expired.
    pub live: bool,
}

/// One inbox account's last poll.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct InboxAccountStatus {
    pub account: String,
    pub kind: String,
    pub last_poll_ms: i64,
    pub last_ok_ms: i64,
    /// Scrubbed, at most 200 characters; empty after a success.
    pub last_error: String,
    pub item_count: i64,
    /// The account's baseline poll has happened.
    pub baselined: bool,
}

impl From<&crate::db_inbox::InboxAccountState> for InboxAccountStatus {
    fn from(s: &crate::db_inbox::InboxAccountState) -> Self {
        InboxAccountStatus {
            account: s.account.clone(),
            kind: s.kind.clone(),
            last_poll_ms: s.last_poll_ms,
            last_ok_ms: s.last_ok_ms,
            last_error: s.last_error.clone(),
            item_count: s.item_count,
            baselined: s.baselined,
        }
    }
}

impl ProducerKindStatus {
    /// A kind's row from its armed flag and (optional) lease at `now_ms`.
    pub fn from_lease(
        kind: crate::producer_lease::ProducerKind,
        armed: bool,
        lease: Option<&crate::producer_lease::LeaseRow>,
        now_ms: i64,
    ) -> Self {
        ProducerKindStatus {
            kind: kind.as_str().to_owned(),
            armed,
            holder: lease.map(|l| l.holder.clone()),
            holder_pid: lease.map(|l| l.holder_pid),
            expires_at_ms: lease.map(|l| l.expires_at_ms),
            live: lease.is_some_and(|l| crate::producer_lease::live(l, now_ms)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::producer_lease::{LeaseRow, ProducerKind};

    #[test]
    fn default_reply_is_an_empty_object_shape() {
        let v = serde_json::to_value(ProducersStatusReply::default()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"installed": false, "headless": false, "kinds": [], "inbox": []})
        );
        let back: ProducersStatusReply = serde_json::from_value(v).unwrap();
        assert_eq!(back, ProducersStatusReply::default());
    }

    #[test]
    fn kind_rows_read_the_lease() {
        let lease = LeaseRow {
            kind: ProducerKind::Inbox,
            holder: "daemon".into(),
            holder_pid: 9,
            acquired_at_ms: 0,
            expires_at_ms: 100,
        };
        let live = ProducerKindStatus::from_lease(ProducerKind::Inbox, true, Some(&lease), 50);
        assert_eq!(live.kind, "inbox");
        assert_eq!(live.holder.as_deref(), Some("daemon"));
        assert_eq!((live.holder_pid, live.expires_at_ms), (Some(9), Some(100)));
        assert!(live.armed && live.live);
        let lapsed = ProducerKindStatus::from_lease(ProducerKind::Inbox, false, Some(&lease), 100);
        assert!(!lapsed.live);
        let none = ProducerKindStatus::from_lease(ProducerKind::Tracker, false, None, 0);
        assert_eq!(none.holder, None);
        assert!(!none.live);
    }

    #[test]
    fn inbox_rows_copy_the_state() {
        let state = crate::db_inbox::InboxAccountState {
            account: "work".into(),
            kind: "imap".into(),
            last_poll_ms: 5,
            last_ok_ms: 4,
            last_error: "e".into(),
            item_count: 3,
            baselined: true,
            ..Default::default()
        };
        let row = InboxAccountStatus::from(&state);
        assert_eq!(
            row,
            InboxAccountStatus {
                account: "work".into(),
                kind: "imap".into(),
                last_poll_ms: 5,
                last_ok_ms: 4,
                last_error: "e".into(),
                item_count: 3,
                baselined: true,
            }
        );
    }
}
