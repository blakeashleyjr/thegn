//! Event bus for aggregating and broadcasting events across the application.
//!
//! The event bus centralizes notifications from multiple sources:
//! - Git events (PR opened/closed/checks failed)
//! - Agent events (dispatch complete/failed)
//! - Test events (failures)
//! - System events (log errors, worktree created)
//!
//! Subscribers can receive events via broadcast channel, and desktop notifications
//! are dispatched for user-visible events.

use serde::{Deserialize, Serialize};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

/// Urgency level for desktop notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NotificationUrgency {
    /// Low urgency: PR state changes, info messages.
    Low,
    /// Normal urgency: mentions, assignments.
    Normal,
    /// Critical urgency: test failures, agent failures, errors.
    Critical,
}

impl NotificationUrgency {
    /// Numeric rank for ordering/threshold comparison (higher = more urgent).
    pub fn rank(self) -> u8 {
        match self {
            Self::Low => 0,
            Self::Normal => 1,
            Self::Critical => 2,
        }
    }

    /// Parse an urgency from a config string (`"low"`, `"normal"`, `"critical"`).
    /// Unknown values fall back to `Normal`.
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "low" => Self::Low,
            "critical" => Self::Critical,
            _ => Self::Normal,
        }
    }

    /// Whether an event at this urgency meets the given threshold.
    pub fn meets(self, threshold: NotificationUrgency) -> bool {
        self.rank() >= threshold.rank()
    }

    /// Get urgency from event type.
    pub fn from_event(event: &Event) -> Self {
        match event {
            Event::PrOpened { .. } => Self::Low,
            Event::PrClosed { .. } => Self::Low,
            Event::PrChecksFailed { .. } => Self::Normal,
            Event::AgentDone { success, .. } => {
                if *success {
                    Self::Low
                } else {
                    Self::Critical
                }
            }
            Event::AgentFailed { .. } => Self::Critical,
            Event::TestsFailed { .. } => Self::Critical,
            Event::LogError { .. } => Self::Critical,
            Event::WorktreeCreated { .. } => Self::Low,
            Event::NotificationReceived { .. } => Self::Normal,
        }
    }
}

/// Desktop notification payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopNotification {
    /// Notification title.
    pub title: String,
    /// Notification body/message.
    pub body: String,
    /// Urgency level.
    pub urgency: NotificationUrgency,
    /// Worktree this notification relates to (may be empty).
    pub worktree: String,
}

impl DesktopNotification {
    /// Create a notification from an event.
    pub fn from_event(event: &Event) -> Option<Self> {
        match event {
            Event::PrOpened {
                pr_number, title, ..
            } => Some(Self {
                title: "PR Opened".into(),
                body: format!("#{}: {}", pr_number, title),
                urgency: NotificationUrgency::Low,
                worktree: String::new(),
            }),
            Event::PrClosed {
                pr_number, merged, ..
            } => Some(Self {
                title: if *merged {
                    "PR Merged".into()
                } else {
                    "PR Closed".into()
                },
                body: format!(
                    "#{} {}",
                    pr_number,
                    if *merged { "merged" } else { "closed" }
                ),
                urgency: NotificationUrgency::Low,
                worktree: String::new(),
            }),
            Event::PrChecksFailed { pr_number, .. } => Some(Self {
                title: "PR Checks Failed".into(),
                body: format!("#{} checks failed", pr_number),
                urgency: NotificationUrgency::Normal,
                worktree: String::new(),
            }),
            Event::AgentDone {
                agent,
                success,
                worktree,
            } => Some(Self {
                title: if *success {
                    "Agent Complete".into()
                } else {
                    "Agent Failed".into()
                },
                body: format!(
                    "{} {}",
                    agent,
                    if *success { "succeeded" } else { "failed" }
                ),
                urgency: if *success {
                    NotificationUrgency::Low
                } else {
                    NotificationUrgency::Critical
                },
                worktree: worktree.clone(),
            }),
            Event::AgentFailed {
                agent,
                error,
                worktree,
            } => Some(Self {
                title: format!("{} Error", agent),
                body: error.clone(),
                urgency: NotificationUrgency::Critical,
                worktree: worktree.clone(),
            }),
            Event::TestsFailed { count, worktree } => Some(Self {
                title: "Tests Failed".into(),
                body: format!(
                    "{} test{} failed",
                    count,
                    if *count == 1 { "" } else { "s" }
                ),
                urgency: NotificationUrgency::Critical,
                worktree: worktree.clone(),
            }),
            Event::LogError { message } => Some(Self {
                title: "Log Error".into(),
                body: message.clone(),
                urgency: NotificationUrgency::Critical,
                worktree: String::new(),
            }),
            Event::WorktreeCreated { branch, .. } => Some(Self {
                title: "Worktree Created".into(),
                body: format!("Created branch: {}", branch),
                urgency: NotificationUrgency::Low,
                worktree: String::new(),
            }),
            Event::NotificationReceived { notification } => Some(Self {
                title: notification.kind.label().into(),
                body: notification.message.clone(),
                urgency: NotificationUrgency::Normal,
                worktree: notification.worktree_path.clone(),
            }),
        }
    }
}

/// Event types that flow through the bus.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Event {
    /// A PR was opened.
    PrOpened {
        worktree: String,
        pr_number: u32,
        title: String,
    },
    /// A PR was closed (merged or not).
    PrClosed {
        worktree: String,
        pr_number: u32,
        merged: bool,
    },
    /// PR checks failed.
    PrChecksFailed { worktree: String, pr_number: u32 },
    /// An agent dispatch finished.
    AgentDone {
        worktree: String,
        agent: String,
        success: bool,
    },
    /// An agent dispatch failed with an error.
    AgentFailed {
        worktree: String,
        agent: String,
        error: String,
    },
    /// Tests failed.
    TestsFailed { worktree: String, count: usize },
    /// A log error was detected.
    LogError { message: String },
    /// A new worktree was created.
    WorktreeCreated { path: String, branch: String },
    /// A notification was received from the DB.
    NotificationReceived {
        notification: crate::notification::Notification,
    },
}

impl Event {
    /// Get the worktree path associated with this event (if any).
    pub fn worktree(&self) -> Option<&str> {
        match self {
            Event::PrOpened { worktree, .. } => Some(worktree),
            Event::PrClosed { worktree, .. } => Some(worktree),
            Event::PrChecksFailed { worktree, .. } => Some(worktree),
            Event::AgentDone { worktree, .. } => Some(worktree),
            Event::AgentFailed { worktree, .. } => Some(worktree),
            Event::TestsFailed { worktree, .. } => Some(worktree),
            Event::LogError { .. } => None,
            Event::WorktreeCreated { path, .. } => Some(path),
            Event::NotificationReceived { notification } => {
                if notification.worktree_path.is_empty() {
                    None
                } else {
                    Some(&notification.worktree_path)
                }
            }
        }
    }
}

/// A subscriber to the event bus.
pub struct EventSubscriber {
    rx: Receiver<Event>,
}

impl EventSubscriber {
    /// Try to receive an event (non-blocking).
    pub fn try_recv(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }

    /// Receive an event (blocking).
    pub fn recv(&self) -> Option<Event> {
        self.rx.recv().ok()
    }
}

/// Internal state for the event bus.
struct EventBusState {
    subscribers: Vec<Sender<Event>>,
    desktop_receivers: Vec<Sender<DesktopNotification>>,
}

/// The event bus - a simple pub/sub for events with desktop notification support.
#[derive(Clone)]
pub struct EventBus {
    #[allow(dead_code)]
    tx: Sender<Event>,
    state: Arc<Mutex<EventBusState>>,
}

impl EventBus {
    /// Create a new event bus.
    pub fn new() -> Self {
        let (tx, _) = mpsc::channel();
        let state = Arc::new(Mutex::new(EventBusState {
            subscribers: Vec::new(),
            desktop_receivers: Vec::new(),
        }));
        Self { tx, state }
    }

    /// Subscribe to events. Returns a subscriber that will receive events.
    pub fn subscribe(&self) -> EventSubscriber {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut state) = self.state.lock() {
            state.subscribers.push(tx);
        }
        EventSubscriber { rx }
    }

    /// Publish an event to all subscribers.
    pub fn publish(&self, event: &Event) {
        if let Ok(state) = self.state.lock() {
            for tx in &state.subscribers {
                let _ = tx.send(event.clone());
            }
        }
    }

    /// Publish an event and also queue a desktop notification if applicable.
    pub fn publish_with_notification(&self, event: &Event) {
        // Always publish the event
        self.publish(event);

        // Queue desktop notification if applicable
        if let Some(notif) = DesktopNotification::from_event(event)
            && let Ok(state) = self.state.lock()
        {
            for tx in &state.desktop_receivers {
                let _ = tx.send(notif.clone());
            }
        }
    }

    /// Get a receiver for desktop notifications.
    pub fn desktop_receiver(&self) -> Receiver<DesktopNotification> {
        let (tx, rx) = mpsc::channel();
        if let Ok(mut state) = self.state.lock() {
            state.desktop_receivers.push(tx);
        }
        rx
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_worktree_extraction() {
        let event = Event::PrOpened {
            worktree: "/wt/app".into(),
            pr_number: 42,
            title: "Test PR".into(),
        };
        assert_eq!(event.worktree(), Some("/wt/app"));

        let event = Event::TestsFailed {
            worktree: "/wt/app".into(),
            count: 3,
        };
        assert_eq!(event.worktree(), Some("/wt/app"));

        let event = Event::LogError {
            message: "error".into(),
        };
        assert_eq!(event.worktree(), None);
    }

    #[test]
    fn desktop_notification_from_event() {
        let event = Event::PrOpened {
            worktree: "/wt/app".into(),
            pr_number: 42,
            title: "Test PR".into(),
        };
        let notif = DesktopNotification::from_event(&event).unwrap();
        assert_eq!(notif.title, "PR Opened");
        assert_eq!(notif.urgency, NotificationUrgency::Low);

        let event = Event::TestsFailed {
            worktree: "/wt/app".into(),
            count: 5,
        };
        let notif = DesktopNotification::from_event(&event).unwrap();
        assert_eq!(notif.title, "Tests Failed");
        assert_eq!(notif.urgency, NotificationUrgency::Critical);
    }

    #[test]
    fn event_bus_publish_subscribe() {
        let bus = EventBus::new();
        let sub = bus.subscribe();

        bus.publish(&Event::PrOpened {
            worktree: "/wt/app".into(),
            pr_number: 1,
            title: "Test".into(),
        });

        // Should receive the event
        let received = sub.try_recv();
        assert!(received.is_some());
    }

    #[test]
    fn urgency_from_event() {
        assert_eq!(
            NotificationUrgency::from_event(&Event::PrOpened {
                worktree: "".into(),
                pr_number: 1,
                title: "".into()
            }),
            NotificationUrgency::Low
        );

        assert_eq!(
            NotificationUrgency::from_event(&Event::TestsFailed {
                worktree: "".into(),
                count: 1
            }),
            NotificationUrgency::Critical
        );
    }

    #[test]
    fn urgency_parse_and_threshold() {
        assert_eq!(NotificationUrgency::parse("low"), NotificationUrgency::Low);
        assert_eq!(
            NotificationUrgency::parse("CRITICAL"),
            NotificationUrgency::Critical
        );
        // Unknown → Normal.
        assert_eq!(
            NotificationUrgency::parse("bogus"),
            NotificationUrgency::Normal
        );

        // Critical meets a normal threshold; low does not.
        assert!(NotificationUrgency::Critical.meets(NotificationUrgency::Normal));
        assert!(NotificationUrgency::Normal.meets(NotificationUrgency::Normal));
        assert!(!NotificationUrgency::Low.meets(NotificationUrgency::Normal));
        // Everything meets a low threshold.
        assert!(NotificationUrgency::Low.meets(NotificationUrgency::Low));
    }

    #[test]
    fn desktop_notification_carries_worktree() {
        let event = Event::AgentFailed {
            worktree: "/wt/app".into(),
            agent: "claude".into(),
            error: "boom".into(),
        };
        let notif = DesktopNotification::from_event(&event).unwrap();
        assert_eq!(notif.worktree, "/wt/app");
        assert_eq!(notif.urgency, NotificationUrgency::Critical);
        assert!(notif.body.contains("boom"));
    }

    #[test]
    fn publish_with_notification_reaches_desktop_receiver() {
        let bus = EventBus::new();
        let desktop_rx = bus.desktop_receiver();
        bus.publish_with_notification(&Event::TestsFailed {
            worktree: "/wt/app".into(),
            count: 2,
        });
        let notif = desktop_rx.try_recv().expect("desktop notification queued");
        assert_eq!(notif.title, "Tests Failed");
        assert_eq!(notif.urgency, NotificationUrgency::Critical);
    }

    #[test]
    fn multiple_subscribers() {
        let bus = EventBus::new();
        let sub1 = bus.subscribe();
        let sub2 = bus.subscribe();

        bus.publish(&Event::PrOpened {
            worktree: "/wt/app".into(),
            pr_number: 1,
            title: "Test".into(),
        });

        // Both subscribers should receive the event
        assert!(sub1.try_recv().is_some());
        assert!(sub2.try_recv().is_some());
    }

    /// Build one of every event variant for exhaustive coverage.
    fn all_events() -> Vec<Event> {
        use crate::notification::{Notification, NotificationKind};
        vec![
            Event::PrOpened {
                worktree: "/wt".into(),
                pr_number: 1,
                title: "t".into(),
            },
            Event::PrClosed {
                worktree: "/wt".into(),
                pr_number: 2,
                merged: true,
            },
            Event::PrClosed {
                worktree: "/wt".into(),
                pr_number: 3,
                merged: false,
            },
            Event::PrChecksFailed {
                worktree: "/wt".into(),
                pr_number: 4,
            },
            Event::AgentDone {
                worktree: "/wt".into(),
                agent: "claude".into(),
                success: true,
            },
            Event::AgentDone {
                worktree: "/wt".into(),
                agent: "claude".into(),
                success: false,
            },
            Event::AgentFailed {
                worktree: "/wt".into(),
                agent: "claude".into(),
                error: "boom".into(),
            },
            Event::TestsFailed {
                worktree: "/wt".into(),
                count: 1,
            },
            Event::LogError {
                message: "err".into(),
            },
            Event::WorktreeCreated {
                path: "/wt".into(),
                branch: "feat".into(),
            },
            Event::NotificationReceived {
                notification: Notification {
                    id: 1,
                    kind: NotificationKind::Assigned,
                    source_ref: "linear:A-1".into(),
                    message: "assigned".into(),
                    created_at_ms: 0,
                    read: false,
                    worktree_path: "/wt".into(),
                },
            },
        ]
    }

    #[test]
    fn every_event_maps_to_a_desktop_notification() {
        for event in all_events() {
            let notif = DesktopNotification::from_event(&event);
            assert!(notif.is_some(), "{event:?} should map to a notification");
            let notif = notif.unwrap();
            assert!(!notif.title.is_empty(), "{event:?} title is empty");
            // Urgency from_event matches the notification's urgency.
            assert_eq!(notif.urgency, NotificationUrgency::from_event(&event));
        }
    }

    #[test]
    fn pr_closed_merged_vs_closed_titles() {
        let merged = DesktopNotification::from_event(&Event::PrClosed {
            worktree: "/wt".into(),
            pr_number: 1,
            merged: true,
        })
        .unwrap();
        assert_eq!(merged.title, "PR Merged");
        let closed = DesktopNotification::from_event(&Event::PrClosed {
            worktree: "/wt".into(),
            pr_number: 1,
            merged: false,
        })
        .unwrap();
        assert_eq!(closed.title, "PR Closed");
    }

    #[test]
    fn agent_done_success_is_low_failure_is_critical() {
        let ok = DesktopNotification::from_event(&Event::AgentDone {
            worktree: "/wt".into(),
            agent: "a".into(),
            success: true,
        })
        .unwrap();
        assert_eq!(ok.urgency, NotificationUrgency::Low);
        assert_eq!(ok.title, "Agent Complete");
        let bad = DesktopNotification::from_event(&Event::AgentDone {
            worktree: "/wt".into(),
            agent: "a".into(),
            success: false,
        })
        .unwrap();
        assert_eq!(bad.urgency, NotificationUrgency::Critical);
        assert_eq!(bad.title, "Agent Failed");
    }

    #[test]
    fn every_event_worktree_accessor() {
        for event in all_events() {
            match &event {
                Event::LogError { .. } => assert_eq!(event.worktree(), None),
                _ => assert!(
                    event.worktree().is_some(),
                    "{event:?} should carry a worktree"
                ),
            }
        }
    }

    #[test]
    fn notification_received_with_empty_worktree_has_none() {
        use crate::notification::{Notification, NotificationKind};
        let event = Event::NotificationReceived {
            notification: Notification {
                id: 1,
                kind: NotificationKind::Mentioned,
                source_ref: "x".into(),
                message: "m".into(),
                created_at_ms: 0,
                read: false,
                worktree_path: String::new(),
            },
        };
        assert_eq!(event.worktree(), None);
        // Still produces a desktop notification (normal urgency).
        let notif = DesktopNotification::from_event(&event).unwrap();
        assert_eq!(notif.urgency, NotificationUrgency::Normal);
    }

    #[test]
    fn blocking_recv_returns_published_event() {
        let bus = EventBus::new();
        let sub = bus.subscribe();
        bus.publish(&Event::LogError {
            message: "x".into(),
        });
        // recv() (blocking) returns the event since one is already queued.
        let ev = sub.recv();
        assert!(matches!(ev, Some(Event::LogError { .. })));
    }

    #[test]
    fn default_bus_constructs() {
        let bus = EventBus::default();
        let sub = bus.subscribe();
        bus.publish(&Event::LogError {
            message: "x".into(),
        });
        assert!(sub.try_recv().is_some());
    }

    #[test]
    fn urgency_ranks_are_ordered() {
        assert!(NotificationUrgency::Low.rank() < NotificationUrgency::Normal.rank());
        assert!(NotificationUrgency::Normal.rank() < NotificationUrgency::Critical.rank());
    }
}
