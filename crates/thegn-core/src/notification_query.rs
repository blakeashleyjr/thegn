//! Notification queries for the Lead (`notifications.list` /
//! `notifications.ack`, THE-2172): filter and acknowledgement DTOs and their
//! pure validation.
//!
//! Stub created by the b152 spine (lane 1); filled by b152 lane 4.

use crate::notification::{Notification, NotificationKind};
use serde::Deserialize;

/// Default and maximum `limit` for `notifications.list`.
pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 500;

/// `notifications.list` request.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct NotificationsListRequest {
    pub kind: Option<String>,
    pub unread: Option<bool>,
    /// Keep rows created at or after this Unix-seconds instant.
    pub since: Option<i64>,
    pub worktree: Option<String>,
    pub limit: Option<usize>,
}

impl NotificationsListRequest {
    /// The effective row cap: default 50, clamped to 1..=500.
    pub fn effective_limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
    }

    /// Validate the `kind` string against [`NotificationKind`].
    pub fn validate(&self) -> Result<Option<NotificationKind>, String> {
        match self.kind.as_deref() {
            None => Ok(None),
            Some(k) => NotificationKind::ALL
                .iter()
                .copied()
                .find(|n| n.as_str() == k)
                .map(Some)
                .ok_or_else(|| format!("unknown notification kind `{k}`")),
        }
    }
}

/// Apply the request's filters (rows are expected newest-first) and the limit.
/// An unknown kind matches nothing; call [`NotificationsListRequest::validate`]
/// first to reject it.
pub fn filter(rows: Vec<Notification>, req: &NotificationsListRequest) -> Vec<Notification> {
    let kind = req.kind.as_deref();
    rows.into_iter()
        .filter(|n| kind.is_none_or(|k| n.kind.as_str() == k))
        .filter(|n| !req.unread.unwrap_or(false) || !n.read)
        .filter(|n| req.since.is_none_or(|s| n.created_at_ms >= s))
        .filter(|n| req.worktree.as_deref().is_none_or(|w| n.worktree_path == w))
        .take(req.effective_limit())
        .map(|mut n| {
            n.message = clip_message(&n.message);
            n
        })
        .collect()
}

/// Maximum characters of a notification message returned by a list reply.
pub const MAX_MESSAGE_CHARS: usize = 1000;

/// Clip `s` to [`MAX_MESSAGE_CHARS`] characters at a char boundary.
pub fn clip_message(s: &str) -> String {
    match s.char_indices().nth(MAX_MESSAGE_CHARS) {
        Some((i, _)) => s[..i].to_owned(),
        None => s.to_owned(),
    }
}

/// `notifications.ack` request; exactly one selector.
#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
pub struct NotificationsAckRequest {
    pub id: Option<i64>,
    pub all: Option<bool>,
    pub worktree: Option<String>,
}

/// What an ack applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AckTarget {
    One(i64),
    All,
    Worktree(String),
}

/// Resolve the single selector; zero or several is an error.
pub fn ack_target(req: &NotificationsAckRequest) -> Result<AckTarget, String> {
    let all = req.all == Some(true);
    let n = usize::from(req.id.is_some()) + usize::from(all) + usize::from(req.worktree.is_some());
    if n != 1 {
        return Err("exactly one of `id`, `all`, `worktree` is required".into());
    }
    if let Some(id) = req.id {
        Ok(AckTarget::One(id))
    } else if all {
        Ok(AckTarget::All)
    } else {
        Ok(AckTarget::Worktree(
            req.worktree.clone().unwrap_or_default(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, kind: NotificationKind, read: bool, at: i64, wt: &str) -> Notification {
        Notification {
            id,
            kind,
            source_ref: String::new(),
            message: String::new(),
            created_at_ms: at,
            read,
            worktree_path: wt.into(),
        }
    }

    fn rows() -> Vec<Notification> {
        vec![
            row(3, NotificationKind::AgentDone, false, 30, "/a"),
            row(2, NotificationKind::TestFailed, true, 20, "/b"),
            row(1, NotificationKind::AgentDone, false, 10, "/a"),
        ]
    }

    #[test]
    fn filters_combine() {
        let ids = |r: &NotificationsListRequest| {
            filter(rows(), r).iter().map(|n| n.id).collect::<Vec<_>>()
        };
        assert_eq!(ids(&NotificationsListRequest::default()), vec![3, 2, 1]);
        let req = |f: fn(&mut NotificationsListRequest)| {
            let mut r = NotificationsListRequest::default();
            f(&mut r);
            r
        };
        assert_eq!(ids(&req(|r| r.unread = Some(true))), vec![3, 1]);
        assert_eq!(ids(&req(|r| r.kind = Some("test_failed".into()))), vec![2]);
        assert_eq!(ids(&req(|r| r.since = Some(20))), vec![3, 2]);
        assert_eq!(ids(&req(|r| r.worktree = Some("/a".into()))), vec![3, 1]);
        assert_eq!(ids(&req(|r| r.limit = Some(1))), vec![3]);
    }

    #[test]
    fn long_messages_are_clipped() {
        let mut long = row(1, NotificationKind::AgentDone, false, 1, "/a");
        long.message = "é".repeat(1500);
        let mut short = row(2, NotificationKind::AgentDone, false, 2, "/a");
        short.message = "hi".into();
        let out = filter(vec![long, short], &NotificationsListRequest::default());
        assert_eq!(out[0].message.chars().count(), 1000);
        assert_eq!(out[1].message, "hi");
    }

    #[test]
    fn limit_defaults_and_clamps() {
        let mut r = NotificationsListRequest::default();
        assert_eq!(r.effective_limit(), 50);
        r.limit = Some(10_000);
        assert_eq!(r.effective_limit(), 500);
        r.limit = Some(0);
        assert_eq!(r.effective_limit(), 1);
    }

    #[test]
    fn unknown_kind_is_an_error() {
        let r = NotificationsListRequest {
            kind: Some("nope".into()),
            ..Default::default()
        };
        assert!(r.validate().is_err());
        let r = NotificationsListRequest {
            kind: Some("agent_done".into()),
            ..Default::default()
        };
        assert_eq!(r.validate().unwrap(), Some(NotificationKind::AgentDone));
    }

    #[test]
    fn ack_target_requires_exactly_one() {
        let none = NotificationsAckRequest::default();
        assert!(ack_target(&none).is_err());
        let one = NotificationsAckRequest {
            id: Some(4),
            ..Default::default()
        };
        assert_eq!(ack_target(&one), Ok(AckTarget::One(4)));
        let all = NotificationsAckRequest {
            all: Some(true),
            ..Default::default()
        };
        assert_eq!(ack_target(&all), Ok(AckTarget::All));
        let wt = NotificationsAckRequest {
            worktree: Some("/w".into()),
            ..Default::default()
        };
        assert_eq!(ack_target(&wt), Ok(AckTarget::Worktree("/w".into())));
        let two = NotificationsAckRequest {
            id: Some(1),
            all: Some(true),
            worktree: None,
        };
        assert!(ack_target(&two).is_err());
        let all_false = NotificationsAckRequest {
            all: Some(false),
            ..Default::default()
        };
        assert!(ack_target(&all_false).is_err());
    }
}
