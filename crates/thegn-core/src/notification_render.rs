//! Pure, provider-neutral notification rendering.
//!
//! Providers choose a [`MarkdownFlavor`] and serialize the resulting value
//! into their platform envelope. This module deliberately has no clock, I/O,
//! runtime, terminal, or configuration access: the caller supplies the event
//! timestamp and all notification data.

use crate::notification::{NotificationKind, Priority};

/// The small set of markup dialects supported by push providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownFlavor {
    CommonMark,
    Discord,
    Slack,
    Plain,
}

/// The immutable result of rendering one notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedNotification {
    pub kind: NotificationKind,
    pub priority: Priority,
    pub source: String,
    pub worktree: String,
    pub timestamp: i64,
    pub title: String,
    pub message: String,
}

impl RenderedNotification {
    /// Render a notification using the caller-supplied timestamp.
    pub fn new(
        kind: NotificationKind,
        priority: Priority,
        message: &str,
        source: &str,
        worktree: &str,
        timestamp: i64,
        flavor: MarkdownFlavor,
    ) -> Self {
        let template = template_for(kind);
        let title = template.title.to_string();
        let body = sanitize(message);
        let body = escape(&body, flavor);
        let message = if body.is_empty() {
            title.clone()
        } else {
            format!("{}: {body}", template.prefix)
        };
        Self {
            kind,
            priority,
            source: sanitize(source),
            worktree: sanitize(worktree),
            timestamp,
            title,
            message,
        }
    }
}

/// The built-in, provider-neutral message vocabulary.  Keeping this table in
/// core makes the event text deterministic for every provider and prevents a
/// provider from inventing a second event taxonomy.  The match is deliberately
/// exhaustive: adding a notification kind requires choosing its push copy.
#[derive(Debug, Clone, Copy)]
struct BuiltinTemplate {
    title: &'static str,
    prefix: &'static str,
}

fn template_for(kind: NotificationKind) -> BuiltinTemplate {
    match kind {
        NotificationKind::Assigned => BuiltinTemplate {
            title: "Assignment",
            prefix: "Assigned issue",
        },
        NotificationKind::Mentioned => BuiltinTemplate {
            title: "Mention",
            prefix: "Mentioned in issue",
        },
        NotificationKind::StatusChanged => BuiltinTemplate {
            title: "Status changed",
            prefix: "Issue status changed",
        },
        NotificationKind::BlockerResolved => BuiltinTemplate {
            title: "Blocker resolved",
            prefix: "Blocker resolved",
        },
        NotificationKind::PrLinked => BuiltinTemplate {
            title: "Pull request linked",
            prefix: "Pull request linked",
        },
        NotificationKind::Overdue => BuiltinTemplate {
            title: "Overdue",
            prefix: "Issue overdue",
        },
        NotificationKind::PrStateChanged => BuiltinTemplate {
            title: "Pull request changed",
            prefix: "Pull request state changed",
        },
        NotificationKind::AgentDone => BuiltinTemplate {
            title: "Agent done",
            prefix: "Agent finished",
        },
        NotificationKind::AgentFailed => BuiltinTemplate {
            title: "Agent failed",
            prefix: "Agent failed",
        },
        NotificationKind::AgentAttention => BuiltinTemplate {
            title: "Agent needs attention",
            prefix: "Agent needs attention",
        },
        NotificationKind::TestFailed => BuiltinTemplate {
            title: "Tests failed",
            prefix: "Tests failed",
        },
        NotificationKind::WorktreeCreated => BuiltinTemplate {
            title: "Worktree created",
            prefix: "Worktree created",
        },
        NotificationKind::LogError => BuiltinTemplate {
            title: "Log error",
            prefix: "Log error",
        },
        NotificationKind::ProcessExited => BuiltinTemplate {
            title: "Process exited",
            prefix: "Process exited",
        },
        NotificationKind::ProcessFailed => BuiltinTemplate {
            title: "Process failed",
            prefix: "Process failed",
        },
        NotificationKind::QueueLanded => BuiltinTemplate {
            title: "Merge queue landed",
            prefix: "Merge queue landed",
        },
        NotificationKind::QueueReady => BuiltinTemplate {
            title: "Merge queue ready to land",
            prefix: "Merge queue ready to land",
        },
        NotificationKind::QueueNeedsHuman => BuiltinTemplate {
            title: "Merge queue needs you",
            prefix: "Merge queue needs you",
        },
        NotificationKind::PrQueueMerged => BuiltinTemplate {
            title: "Pull request merged",
            prefix: "Pull request merged",
        },
        NotificationKind::PrQueueReady => BuiltinTemplate {
            title: "Pull request ready to merge",
            prefix: "Pull request ready to merge",
        },
        NotificationKind::PrQueueNeedsHuman => BuiltinTemplate {
            title: "PR queue needs you",
            prefix: "PR queue needs you",
        },
        NotificationKind::CalendarReminder => BuiltinTemplate {
            title: "Event starting soon",
            prefix: "Event starting soon",
        },
        NotificationKind::CalendarChanged => BuiltinTemplate {
            title: "Event changed",
            prefix: "Calendar event changed",
        },
        NotificationKind::UpstreamBehind => BuiltinTemplate {
            title: "Upstream updates",
            prefix: "Upstream updates available",
        },
        NotificationKind::ResourceAlert => BuiltinTemplate {
            title: "Resource alert",
            prefix: "Resource alert",
        },
        NotificationKind::UsageLimit => BuiltinTemplate {
            title: "Usage limit",
            prefix: "Usage limit approaching",
        },
    }
}

/// Convenience wrapper around [`RenderedNotification::new`].
pub fn render(
    kind: NotificationKind,
    priority: Priority,
    message: &str,
    source: &str,
    worktree: &str,
    timestamp: i64,
    flavor: MarkdownFlavor,
) -> RenderedNotification {
    RenderedNotification::new(kind, priority, message, source, worktree, timestamp, flavor)
}

/// Truncate by Unicode scalar values, keeping the visible marker inside the
/// bound. A marker longer than the bound is itself truncated safely.
pub fn truncate_chars(value: &str, max_chars: usize, marker: &str) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let marker: String = marker.chars().take(max_chars).collect();
    let keep = max_chars.saturating_sub(marker.chars().count());
    let prefix: String = value.chars().take(keep).collect();
    format!("{prefix}{marker}")
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter_map(|c| match c {
            '\r' => Some('\n'),
            '\n' | '\t' => Some(c),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect()
}

fn escape(value: &str, flavor: MarkdownFlavor) -> String {
    match flavor {
        MarkdownFlavor::Plain => value.to_string(),
        MarkdownFlavor::CommonMark => escape_chars(value, r"\\*_[]()#~`>"),
        MarkdownFlavor::Discord => escape_chars(value, "\\*_~|>`"),
        // Slack's mrkdwn treats ampersand and angle brackets as control
        // syntax, so encode them while leaving its emphasis markers usable.
        MarkdownFlavor::Slack => value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;"),
    }
}

fn escape_chars(value: &str, chars: &str) -> String {
    value
        .chars()
        .map(|c| {
            if chars.contains(c) {
                format!("\\{c}")
            } else {
                c.to_string()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_has_a_builtin_template() {
        for kind in NotificationKind::ALL {
            let rendered = render(
                kind,
                Priority::Notice,
                "hello",
                "src",
                "/wt",
                42,
                MarkdownFlavor::Plain,
            );
            let template = template_for(kind);
            assert!(!template.title.is_empty(), "{kind:?}");
            assert!(!template.prefix.is_empty(), "{kind:?}");
            assert_eq!(rendered.title, template.title, "{kind:?}");
            assert!(rendered.message.contains(template.prefix), "{kind:?}");
        }
    }

    #[test]
    fn event_templates_are_not_one_generic_shape() {
        let assigned = render(
            NotificationKind::Assigned,
            Priority::Notice,
            "ABC-1",
            "linear:ABC-1",
            "/repo",
            42,
            MarkdownFlavor::Plain,
        );
        let failed = render(
            NotificationKind::TestFailed,
            Priority::Alert,
            "ABC-1",
            "ci:run",
            "/repo",
            42,
            MarkdownFlavor::Plain,
        );
        assert_eq!(assigned.message, "Assigned issue: ABC-1");
        assert_eq!(failed.message, "Tests failed: ABC-1");
        assert_ne!(assigned.title, failed.title);
    }

    #[test]
    fn flavors_escape_their_control_syntax() {
        let input = "a*b _c_ [d] <e> & `f`\nnext\0";
        let common = render(
            NotificationKind::Mentioned,
            Priority::Notice,
            input,
            "s",
            "w",
            1,
            MarkdownFlavor::CommonMark,
        );
        assert!(common.message.contains(r"a\*b"));
        let discord = render(
            NotificationKind::Mentioned,
            Priority::Notice,
            input,
            "s",
            "w",
            1,
            MarkdownFlavor::Discord,
        );
        assert!(discord.message.contains(r"a\*b"));
        let slack = render(
            NotificationKind::Mentioned,
            Priority::Notice,
            input,
            "s",
            "w",
            1,
            MarkdownFlavor::Slack,
        );
        assert!(slack.message.contains("&lt;e&gt;") && slack.message.contains("&amp;"));
        let plain = render(
            NotificationKind::Mentioned,
            Priority::Notice,
            input,
            "s",
            "w",
            1,
            MarkdownFlavor::Plain,
        );
        assert!(plain.message.contains("a*b") && !plain.message.contains('\0'));
    }

    #[test]
    fn truncation_counts_unicode_and_keeps_marker_visible() {
        let result = truncate_chars("你好世界🌍!", 5, "…");
        assert_eq!(result, "你好世界…");
        assert_eq!(result.chars().count(), 5);
        assert_eq!(truncate_chars("abc", 2, "…"), "a…");
    }

    #[test]
    fn generic_fields_are_stable_inputs() {
        let rendered = render(
            NotificationKind::AgentDone,
            Priority::Alert,
            "done",
            "linear:ABC-1",
            "/repo",
            1_700_000_000,
            MarkdownFlavor::Plain,
        );
        assert_eq!(rendered.kind, NotificationKind::AgentDone);
        assert_eq!(rendered.priority, Priority::Alert);
        assert_eq!(rendered.source, "linear:ABC-1");
        assert_eq!(rendered.worktree, "/repo");
        assert_eq!(rendered.timestamp, 1_700_000_000);
    }
}
