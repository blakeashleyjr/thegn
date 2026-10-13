//! Pure confirmation primitives shared by destructive panel actions.

use std::time::{Duration, Instant};

use crate::panel::Section;

const CONFIRM_WINDOW: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub struct TwoPress {
    armed: Option<(String, Instant)>,
}

impl TwoPress {
    pub fn confirm(&mut self, key: &str, now: Instant) -> bool {
        let confirmed = self.armed.as_ref().is_some_and(|(armed_key, armed_at)| {
            armed_key == key && now.saturating_duration_since(*armed_at) < CONFIRM_WINDOW
        });
        if confirmed {
            self.armed = None;
            true
        } else {
            self.armed = Some((key.to_owned(), now));
            false
        }
    }

    pub fn clear(&mut self) {
        self.armed = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardKind {
    TwoPress,
    Menu(&'static str),
}

pub const GUARDED: &[(Section, char, GuardKind)] = &[
    (Section::PrQueue, 'c', GuardKind::TwoPress),
    (Section::MergeQueue, 'c', GuardKind::TwoPress),
    (Section::Ci, 'c', GuardKind::TwoPress),
    (Section::Pr, 'M', GuardKind::Menu("pr-merge")),
    (
        Section::Notifications,
        'a',
        GuardKind::Menu("notifications-clear-all"),
    ),
    (Section::Sandbox, 's', GuardKind::Menu("sandbox-stop")),
    (Section::Sandbox, 'r', GuardKind::Menu("sandbox-restart")),
    (Section::Environments, 'x', GuardKind::Menu("env-remove")),
    (Section::Hosts, 'x', GuardKind::Menu("host-rm")),
];

pub fn arm_prompt(key: char, what: &str) -> String {
    format!("press {key} again within 3 s to {what}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_press_needs_the_same_key_within_the_window() {
        let t0 = Instant::now();
        let mut guard = TwoPress::default();
        assert!(!guard.confirm("a", t0));
        assert!(guard.confirm("a", t0 + Duration::from_secs(1)));
        assert!(!guard.confirm("a", t0 + Duration::from_secs(2)));
        assert!(!guard.confirm("b", t0 + Duration::from_secs(3)));
        assert!(!guard.confirm("a", t0 + Duration::from_secs(4)));
        assert!(!guard.confirm("a", t0 + Duration::from_secs(8)));
    }

    #[test]
    fn arm_prompt_names_the_key() {
        assert_eq!(
            arm_prompt('c', "clear the queue"),
            "press c again within 3 s to clear the queue"
        );
    }
}
